//! AFT-owned environment and files for first-party agent children.
//!
//! The governance controls in this module are attached to spawned bash and PTY
//! children. AFT never edits the user's shell startup files or global Git
//! configuration, so an operator's terminal keeps its existing behavior.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;

pub const SHIMS_DIR_NAME: &str = "shims";
pub const GIT_HOOKS_DIR_NAME: &str = "git-hooks";
const ALFONSO_TEST_THREADS_FILE: &str = "alfonso-test-threads";
const DEFAULT_WORKER_TEST_THREADS: u32 = 4;
const MAX_WORKER_TEST_THREADS: u32 = 256;
const NEXT_TEST_THREADS_ENV: &str = "NEXTEST_TEST_THREADS";
const RUST_TEST_THREADS_ENV: &str = "RUST_TEST_THREADS";
const GIT_HOOKS_QUARANTINE_DIR_NAME: &str = "quarantine";
const PREPARE_COMMIT_MSG: &str = "prepare-commit-msg";
// This is the complete hook inventory documented by `githooks(5)`, including
// receive-side and specialized hooks. Agent Git can operate on bare repositories
// and invoke less-common porcelain, so limiting dispatch to commit hooks would
// silently disable repository policy for those operations.
const MANAGED_GIT_HOOK_NAMES: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    PREPARE_COMMIT_MSG,
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "pre-receive",
    "update",
    "proc-receive",
    "post-receive",
    "post-update",
    "push-to-checkout",
    "pre-auto-gc",
    "post-rewrite",
    "sendemail-validate",
    "fsmonitor-watchman",
    "p4-changelist",
    "p4-prepare-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "post-index-change",
    "reference-transaction",
];
const GH_SHIMS_DIR_ENV: &str = "AFT_GH_SHIMS_DIR";
const GH_SHIM_BINARY_ENV: &str = "AFT_GH_SHIM_BINARY";
const GIT_CO_AUTHOR_ENV: &str = "AFT_GIT_CO_AUTHOR";
const STORAGE_DIR_ENV: &str = "AFT_STORAGE_DIR";
const GH_SHIM_STATE_DIR_ENV: &str = "AFT_GH_SHIM_STATE_DIR";
const SUBC_CREDENTIAL_ENV_PREFIX: &str = "SUBC_";
const SUBC_IDENTITY_ENV_KEYS: [&str; 3] = [
    subc_protocol::SUBC_MODULE_ID_ENV,
    subc_protocol::SUBC_LAUNCH_NONCE_ENV,
    // Names the descriptor the daemon passed the nonce through. It is removed
    // along with the nonce: a child that inherited this variable without the
    // pipe would read and close whatever unrelated descriptor it has at that
    // number.
    subc_os::launch_nonce::LAUNCH_NONCE_FD_ENV,
];

static WORKER_TEST_THREAD_LOGGED_WORKTREES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static WORKER_TEST_THREAD_OVERSIZE_LOGGED_WORKTREES: OnceLock<Mutex<HashSet<PathBuf>>> =
    OnceLock::new();

#[derive(Debug, PartialEq, Eq)]
enum WorkerTestThreadBudgetIssue {
    Invalid(&'static str),
    Oversized(String),
}

/// Supply bounded test-run concurrency to a worker-preset bash child without
/// replacing a value the caller already chose.
pub(crate) fn inject_worker_test_threads(
    project_root: &Path,
    environment: &mut HashMap<String, String>,
) {
    inject_worker_test_threads_with(project_root, environment, |name| {
        std::env::var_os(name).is_some()
    });
}

fn inject_worker_test_threads_with(
    project_root: &Path,
    environment: &mut HashMap<String, String>,
    inherited_has: impl Fn(&str) -> bool,
) {
    let missing = [NEXT_TEST_THREADS_ENV, RUST_TEST_THREADS_ENV]
        .into_iter()
        .filter(|name| !has_test_thread_override(environment, name) && !inherited_has(name))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return;
    }

    let (threads, issue) = worker_test_thread_budget(project_root);
    if let Some(issue) = issue {
        log_worker_test_thread_fallback_once(project_root, issue);
    }
    let threads = threads.to_string();
    for name in missing {
        environment.insert(name.to_string(), threads.clone());
    }
}

fn has_test_thread_override(environment: &HashMap<String, String>, name: &str) -> bool {
    #[cfg(windows)]
    {
        environment.keys().any(|key| key.eq_ignore_ascii_case(name))
    }
    #[cfg(not(windows))]
    {
        environment.contains_key(name)
    }
}

fn worker_test_thread_budget_path(project_root: &Path) -> Option<PathBuf> {
    project_root.parent().map(|worktree_parent| {
        worktree_parent
            .join(".cargo")
            .join(ALFONSO_TEST_THREADS_FILE)
    })
}

/// Reject oversized budgets instead of letting a corrupt file overwhelm the
/// shared machine; valid values are always between one and 256 threads.
fn worker_test_thread_budget(project_root: &Path) -> (u32, Option<WorkerTestThreadBudgetIssue>) {
    let Some(path) = worker_test_thread_budget_path(project_root) else {
        return (
            DEFAULT_WORKER_TEST_THREADS,
            Some(WorkerTestThreadBudgetIssue::Invalid(
                "worktree has no parent directory",
            )),
        );
    };
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(_) => {
            return (
                DEFAULT_WORKER_TEST_THREADS,
                Some(WorkerTestThreadBudgetIssue::Invalid(
                    "budget file is absent or unreadable",
                )),
            )
        }
    };
    let Ok(contents) = std::str::from_utf8(&contents) else {
        return (
            DEFAULT_WORKER_TEST_THREADS,
            Some(WorkerTestThreadBudgetIssue::Invalid(
                "budget file is not valid UTF-8",
            )),
        );
    };
    let Some(digits) = contents.strip_suffix('\n') else {
        return (
            DEFAULT_WORKER_TEST_THREADS,
            Some(WorkerTestThreadBudgetIssue::Invalid(
                "budget file must contain a decimal integer and newline",
            )),
        );
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return (
            DEFAULT_WORKER_TEST_THREADS,
            Some(WorkerTestThreadBudgetIssue::Invalid(
                "budget file does not contain a positive decimal integer",
            )),
        );
    }
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return (
            DEFAULT_WORKER_TEST_THREADS,
            Some(WorkerTestThreadBudgetIssue::Invalid(
                "budget file does not contain a positive decimal integer",
            )),
        );
    }
    if significant.len() > 3 || (significant.len() == 3 && significant > "256") {
        return (
            DEFAULT_WORKER_TEST_THREADS,
            Some(WorkerTestThreadBudgetIssue::Oversized(digits.to_string())),
        );
    }
    match significant.parse::<u32>() {
        Ok(threads) => (threads, None),
        Err(_) => (
            DEFAULT_WORKER_TEST_THREADS,
            Some(WorkerTestThreadBudgetIssue::Invalid(
                "budget file does not contain a positive decimal integer",
            )),
        ),
    }
}

fn log_worker_test_thread_fallback_once(worktree: &Path, issue: WorkerTestThreadBudgetIssue) {
    match issue {
        WorkerTestThreadBudgetIssue::Invalid(reason) => {
            let logged = WORKER_TEST_THREAD_LOGGED_WORKTREES.get_or_init(Default::default);
            let Ok(mut logged) = logged.lock() else {
                return;
            };
            if logged.insert(worktree.to_path_buf()) {
                log::debug!(
                    "using {DEFAULT_WORKER_TEST_THREADS} test threads for worker bash in {}: {reason}",
                    worktree.display()
                );
            }
        }
        WorkerTestThreadBudgetIssue::Oversized(value) => {
            let logged = WORKER_TEST_THREAD_OVERSIZE_LOGGED_WORKTREES.get_or_init(Default::default);
            let Ok(mut logged) = logged.lock() else {
                return;
            };
            if logged.insert(worktree.to_path_buf()) {
                let path = worker_test_thread_budget_path(worktree)
                    .unwrap_or_else(|| PathBuf::from(ALFONSO_TEST_THREADS_FILE));
                log::warn!(
                    "worker test thread budget file {} contains value {}; using {DEFAULT_WORKER_TEST_THREADS} threads for {} because budgets above {MAX_WORKER_TEST_THREADS} are invalid",
                    path.display(),
                    value,
                    worktree.display()
                );
            }
        }
    }
}

/// Git for Windows runs shebang hooks through its bundled POSIX shell, so the
/// same dispatcher bytes work there and on Unix. Only the repository hook
/// receives Git's stdin; resolver probes have closed input and a deadline.
const GIT_HOOK_DISPATCHER_TEMPLATE: &str = r#"#!/bin/sh
# AFT selects this hook through the agent child's environment. It does not alter
# the repository or the user's Git configuration.
hook_name=@HOOK_NAME@
# A worktree hook may have recorded this dispatcher as its original hooks path
# and exec back to it. File identity alone cannot detect that indirect cycle.
# Exec preserves the PID; a separate Git invocation from a hook has a new PID
# and must still run its own repository policy.
if [ "${AFT_GIT_HOOK_DISPATCH_OWNER:-}" = "$$:$hook_name" ]; then
  reentered=1
else
  reentered=
  AFT_GIT_HOOK_DISPATCH_OWNER="$$:$hook_name"
  export AFT_GIT_HOOK_DISPATCH_OWNER
fi

bounded_probe() (
  # Resolver commands do not consume hook input. Keep the watchdog's output out
  # of command-substitution pipes, and reap it on success rather than waiting
  # for the whole deadline or leaving its sleep behind.
  "$@" </dev/null &
  probe_pid=$!
  (
    # The shell publishes $! when it starts its only background child, before
    # running a pending trap. Do not copy it to a variable: cancellation can
    # arrive between starting sleep and assigning that variable. Before sleep
    # starts, $! is inherited from the enclosing shell and names the probe, not
    # a child of this watchdog; cancellation then just exits without a timer.
    # Use SIGKILL for the timer: TERM can arrive in the forked shell before it
    # execs sleep and be consumed by that shell's inherited trap instead. Waiting
    # for that still-live timer would delay a completed probe for ten seconds.
    trap 'trap "" TERM; if [ "$!" != "$probe_pid" ]; then kill -KILL "$!" 2>/dev/null; wait "$!" 2>/dev/null; fi; exit 0' TERM
    sleep 10 &
    wait "$!"
    kill -KILL "$probe_pid" 2>/dev/null
  ) </dev/null >/dev/null 2>&1 &
  watchdog_pid=$!
  # SIGKILL cannot run shell cleanup. If the hook itself is killed externally,
  # this subshell and its watchdog still finish within the ten-second deadline.
  wait "$probe_pid" 2>/dev/null
  probe_status=$?
  kill "$watchdog_pid" 2>/dev/null || :
  wait "$watchdog_pid" 2>/dev/null || :
  # Git uses 128 for ordinary errors, including --show-toplevel in a bare
  # repository. Only the watchdog's SIGKILL status denotes a deadline here.
  if [ "$probe_status" -eq 137 ]; then
    return 124
  fi
  return "$probe_status"
)

probe_value() {
  bounded_probe "$@" 2>/dev/null
  probe_status=$?
  # An absent config key is normal; a stalled resolver is not. Do not turn a
  # timeout into an empty path that silently skips repository policy.
  if [ "$probe_status" -eq 124 ]; then
    printf '%s\n' "AFT: $hook_name resolver exceeded its deadline" >&2
    return 124
  fi
  return 0
}
if [ -z "$reentered" ]; then
  : # Most hooks have no attribution pre-dispatch step.
@PRE_DISPATCH@
fi
dispatch_candidate() {
  candidate=$1
  shift
  if [ -x "$candidate" ]; then
    # A repository may explicitly point core.hooksPath back at AFT's managed
    # directory. Identity comparison also catches symlink and hard-link loops.
    if [ "$candidate" -ef "$0" ] 2>/dev/null; then
      return
    fi
    exec "$candidate" "$@"
  fi
}

repo_root=$(probe_value git rev-parse --show-toplevel) || exit 1
if [ -z "$repo_root" ]; then
  repo_root=$(probe_value git rev-parse --absolute-git-dir) || exit 1
fi
if [ -z "$repo_root" ]; then
  exit 0
fi

# Resolve core.hooksPath from configuration scopes in descending precedence:
# worktree > local > global > system. We query each scope explicitly because
# the ambient environment injects a command-scope core.hooksPath pointing to this
# dispatcher; scoped queries exclude command-scope overrides and reflect the
# hook path Git would have resolved natively.
configured_hooks=
if [ -z "$reentered" ]; then
for scope in --worktree --local --global --system; do
  configured_hooks=$(probe_value git config "$scope" --get core.hooksPath) || exit 1
  if [ -n "$configured_hooks" ]; then
    break
  fi
done

if [ -n "$configured_hooks" ]; then
  case "$configured_hooks" in
    /*|[A-Za-z]:[\\/]*) candidate="$configured_hooks/$hook_name" ;;
    \~/*) candidate="${HOME:-}${configured_hooks#\~}/$hook_name" ;;
    *) candidate="$repo_root/$configured_hooks/$hook_name" ;;
  esac
  dispatch_candidate "$candidate" "$@"
  # When core.hooksPath is configured, Git checks only that directory and does
  # not fall back to the default hooks directory if the hook is absent.
  exit 0
fi
fi

# Do not use `git rev-parse --git-path hooks/...` here: it honors the injected
# core.hooksPath and resolves this dispatcher back to itself.
# On reentry the configured directory has already run its policy. Complete its
# chain with Git's native common-directory hooks, not the configured wrapper
# again. A native hook can itself chain to AFT, so visit that fallback only once
# in this exec chain. Separate Git processes do not share this PID identity.
if [ "${AFT_GIT_HOOK_NATIVE_OWNER:-}" = "$$:$hook_name" ]; then
  exit 0
fi
AFT_GIT_HOOK_NATIVE_OWNER="$$:$hook_name"
export AFT_GIT_HOOK_NATIVE_OWNER
git_dir=$(probe_value git rev-parse --git-common-dir) || exit 1
if [ -z "$git_dir" ]; then
  git_dir=$(probe_value git rev-parse --git-dir) || exit 1
fi
if [ -n "$git_dir" ]; then
  case "$git_dir" in
    /*|[A-Za-z]:[\\/]*) candidate="$git_dir/hooks/$hook_name" ;;
    *) candidate="$repo_root/$git_dir/hooks/$hook_name" ;;
  esac
  dispatch_candidate "$candidate" "$@"
fi

# Git does not search .githooks natively; do not add a second policy directory
# when completing an already-selected repository hook's chain.
if [ -n "$reentered" ]; then
  exit 0
fi
dispatch_candidate "$repo_root/.githooks/$hook_name" "$@"
exit 0
"#;

const PREPARE_COMMIT_MSG_PRE_DISPATCH: &str = r#"# Agent-labeled commits are joint work too, so subjects such as "mason:" do not
# receive an attribution exemption. Attribution runs before the repository hook
# so that hook can validate or amend the resulting message.
msg_file=$1
mode=${AFT_GIT_CO_AUTHOR:-off}
line=

case "$mode" in
  off|'') ;;
  auto)
    if [ -n "${AFT_GH_SHIM_BINARY:-}" ]; then
      line=$(probe_value "$AFT_GH_SHIM_BINARY" gh-shim --co-author-line) || exit 1
    fi
    ;;
  *) line="Co-authored-by: $mode" ;;
esac

if [ -n "$line" ]; then
  identity=${line#Co-authored-by: }
  probe_value git interpret-trailers --in-place --if-exists doNothing \
    --trailer "Co-authored-by=$identity" "$msg_file" || exit 1
fi
"#;

/// Length, in hex characters, of the content key that names one hook set.
const GIT_HOOK_SET_KEY_LEN: usize = 32;

/// The complete dispatcher set and the key derived from its bytes. Every hook
/// name and body feeds the key, so any template change yields a new directory
/// while identical content always resolves to the same one. Older keys remain
/// valid aliases: maintenance upgrades AFT-owned scripts there in place because
/// worktree managers may have recorded those paths in their own hook chains.
struct ManagedGitHookSet {
    hooks: Vec<(&'static str, String)>,
    key: String,
}

fn managed_git_hook_set() -> &'static ManagedGitHookSet {
    static SET: OnceLock<ManagedGitHookSet> = OnceLock::new();
    SET.get_or_init(|| {
        let hooks = MANAGED_GIT_HOOK_NAMES
            .iter()
            .map(|name| (*name, managed_git_hook_contents(name)))
            .collect::<Vec<_>>();
        let mut hasher = blake3::Hasher::new();
        for (name, contents) in &hooks {
            // Length-prefix both fields so no two different sets can serialize
            // to the same byte stream.
            hasher.update(&(name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update(&(contents.len() as u64).to_le_bytes());
            hasher.update(contents.as_bytes());
        }
        let key = hasher.finalize().to_hex()[..GIT_HOOK_SET_KEY_LEN].to_string();
        ManagedGitHookSet { hooks, key }
    })
}

/// Directory that git's `core.hooksPath` points at for governed children.
///
/// The dispatchers are identical for every storage root, so they live once per
/// user in a directory named by their content key rather than once per storage
/// root. macOS scans every newly created executable on its first run; one
/// shared copy costs one scan per hook version instead of one per storage root
/// (and, in the test suite, one per test).
pub fn managed_git_hooks_dir(storage_root: &Path) -> PathBuf {
    managed_git_hooks_root_from(
        crate::environment::non_empty_os_var,
        cfg!(windows),
        storage_root,
    )
    .join(&managed_git_hook_set().key)
}

/// Parent of the content-keyed hook directories. It follows the per-user AFT
/// cache root the TypeScript side already uses for downloaded binaries
/// (`getAftCacheRoot`): `AFT_CACHE_DIR` when set, then `%LOCALAPPDATA%\aft`
/// on Windows, otherwise `$XDG_CACHE_HOME/aft` or `~/.cache/aft`. Only absolute
/// values count, so a relative variable cannot move hooks under whatever
/// directory a child happens to start in. With no usable home at all the set
/// stays under the storage root, where it lived before.
///
/// The test gate (`scripts/rust-test-gate.sh`) exports one `XDG_CACHE_HOME`
/// for the whole run, so every test shares the same hook set too.
fn managed_git_hooks_root_from(
    lookup: impl Fn(&str) -> Option<OsString>,
    windows: bool,
    storage_root: &Path,
) -> PathBuf {
    let absolute = |name: &str| {
        lookup(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let cache_root = absolute("AFT_CACHE_DIR").or_else(|| {
        let base = if windows {
            absolute("LOCALAPPDATA")
                .or_else(|| absolute("APPDATA"))
                .or_else(|| {
                    absolute("USERPROFILE")
                        .or_else(|| absolute("HOME"))
                        .map(|home| home.join("AppData").join("Local"))
                })
        } else {
            absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|home| home.join(".cache")))
        };
        base.map(|base| base.join("aft"))
    });
    cache_root
        .unwrap_or_else(|| storage_root.to_path_buf())
        .join(GIT_HOOKS_DIR_NAME)
}

/// True when a `core.hooksPath` value was selected by AFT: either a
/// content-keyed hook directory, or the per-storage-root directory that older
/// daemons injected (an inherited environment may still carry that value).
fn is_managed_git_hooks_path(value: &Path) -> bool {
    if value.ends_with(GIT_HOOKS_DIR_NAME) {
        return true;
    }
    let keyed = value
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.len() == GIT_HOOK_SET_KEY_LEN && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    keyed
        && value
            .parent()
            .is_some_and(|parent| parent.ends_with(GIT_HOOKS_DIR_NAME))
}

fn managed_git_hook_contents(hook_name: &str) -> String {
    let pre_dispatch = if hook_name == PREPARE_COMMIT_MSG {
        PREPARE_COMMIT_MSG_PRE_DISPATCH
    } else {
        ""
    };
    GIT_HOOK_DISPATCHER_TEMPLATE
        .replace("@HOOK_NAME@", hook_name)
        .replace("@PRE_DISPATCH@", pre_dispatch)
}

/// Refresh files selected by the resolved configuration. This runs during
/// configure and is also cheap enough to repair a stale entry immediately
/// before a child spawn.
pub fn maintain(config: &Config, storage_root: &Path) -> Result<(), String> {
    let shims_dir = storage_root.join(SHIMS_DIR_NAME);
    if config.github.shim {
        let binary = shim_binary(config)?;
        match reject_self_referential_pin(&binary, &shims_dir)
            .and_then(|()| probe_gh_shim_binary(&binary))
        {
            Ok(()) => ensure_gh_entry(&shims_dir, &binary)?,
            Err(reason) => {
                crate::slog_warn!(
                    "[agent_child_env] refusing gh shim candidate {}: {reason}",
                    binary.display()
                );
                if !existing_gh_entry_is_valid(&shims_dir) {
                    remove_gh_entry(&shims_dir)?;
                    crate::slog_warn!(
                        "[agent_child_env] removed unverified gh shim entry after refusing candidate {}",
                        binary.display()
                    );
                }
            }
        }
    } else {
        remove_gh_entry(&shims_dir)?;
    }

    if config.git.co_author != "off" {
        ensure_managed_git_hooks(&managed_git_hooks_dir(storage_root))?;
    }
    Ok(())
}

/// Install the managed git hooks that `git.co_author` needs, without touching
/// the gh shim. A live config reload turning co-author attribution on calls
/// this; the shim is only changed by a full configure.
pub(crate) fn ensure_git_hooks(storage_root: &Path) -> Result<(), String> {
    ensure_managed_git_hooks(&managed_git_hooks_dir(storage_root))
}

/// Remove inherited governance markers from THIS PROCESS's environment.
///
/// A daemon is the injector of these markers, never a consumer: when an agent
/// whose own environment was governed by an outer daemon spawns a nested aft
/// process (test harnesses, tooling, warmup), the inherited markers would leak
/// into every child this process spawns regardless of this process's own
/// configuration gates. Called once at server startup, before threads spawn;
/// the gh-shim invocation path (which legitimately reads the shims marker)
/// dispatches before this runs.
pub fn scrub_inherited_process_markers() {
    if let Some(stale) = crate::environment::non_empty_os_var(GH_SHIMS_DIR_ENV).map(PathBuf::from) {
        if let Some(inherited) = std::env::var_os("PATH") {
            let cleaned: Vec<_> = std::env::split_paths(&inherited)
                .filter(|entry| entry != &stale)
                .collect();
            if let Ok(path) = std::env::join_paths(cleaned) {
                std::env::set_var("PATH", path);
            }
        }
        std::env::remove_var(GH_SHIMS_DIR_ENV);
    }
    std::env::remove_var(GIT_CO_AUTHOR_ENV);
    std::env::remove_var(GH_SHIM_BINARY_ENV);
    let aft_hooks_value = std::env::var_os("GIT_CONFIG_VALUE_0")
        .is_some_and(|value| is_managed_git_hooks_path(Path::new(&value)));
    if aft_hooks_value
        && std::env::var_os("GIT_CONFIG_KEY_0").as_deref()
            == Some(std::ffi::OsStr::new("core.hooksPath"))
    {
        std::env::remove_var("GIT_CONFIG_COUNT");
        std::env::remove_var("GIT_CONFIG_KEY_0");
        std::env::remove_var("GIT_CONFIG_VALUE_0");
    }
}

/// True for environment variables reserved for subc's supervised-spawn
/// identity. Tool children are not the module process and must never inherit
/// present or future members of this credential family.
pub(crate) fn is_subc_credential_env_key(key: &str) -> bool {
    #[cfg(windows)]
    {
        key.as_bytes()
            .get(..SUBC_CREDENTIAL_ENV_PREFIX.len())
            .is_some_and(|prefix| {
                prefix.eq_ignore_ascii_case(SUBC_CREDENTIAL_ENV_PREFIX.as_bytes())
            })
    }
    #[cfg(not(windows))]
    {
        key.starts_with(SUBC_CREDENTIAL_ENV_PREFIX)
    }
}

/// Apply request overrides to a non-PTY child and remove subc credentials from
/// both the inherited process environment and explicit command overrides.
pub(crate) fn apply_to_command(command: &mut Command, environment: &HashMap<String, String>) {
    // A ticket in this process's own environment belongs to whatever command
    // started this process, never to the child. The child gets a ticket only
    // when its request environment carries one issued for it.
    if !environment.contains_key(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV) {
        command.env_remove(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV);
    }
    command.envs(environment);
    command.env_remove(crate::privacy_spawn::CONTROL_ENV);

    let mut credential_keys = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_str().is_some_and(is_subc_credential_env_key))
        .collect::<Vec<_>>();
    credential_keys.extend(
        command
            .get_envs()
            .map(|(key, _)| key.to_os_string())
            .filter(|key| key.to_str().is_some_and(is_subc_credential_env_key)),
    );
    for key in credential_keys {
        command.env_remove(key);
    }
}

/// Remove subc credentials from portable-pty's complete environment snapshot.
/// CommandBuilder materializes the process environment when it is constructed,
/// so filtering the builder covers Unix exec and Windows CreateProcess alike.
pub(crate) fn scrub_pty_command(command: &mut portable_pty::CommandBuilder) {
    command.env_remove(crate::privacy_spawn::CONTROL_ENV);
    let mut credential_keys = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_str().is_some_and(is_subc_credential_env_key))
        .collect::<Vec<_>>();
    credential_keys.extend(
        command
            .iter_full_env_as_str()
            .map(|(key, _)| OsString::from(key))
            .filter(|key| key.to_str().is_some_and(is_subc_credential_env_key)),
    );
    credential_keys.extend(SUBC_IDENTITY_ENV_KEYS.map(OsString::from));
    for key in credential_keys {
        command.env_remove(key);
    }
}

/// Add governance to one child environment. This is the single seam used
/// before foreground, background, sandboxed, and PTY launch planning.
///
/// `gh_shim_ticket` is the per-command ticket from [`crate::gh_shim_ticket`]
/// that lets this child's `gh` shim ask the daemon to relay a bot write for
/// the session that spawned it. An inherited ticket is always removed first, so
/// a child never speaks with a ticket that was issued to some other command.
pub fn inject(
    config: &Config,
    storage_root: &Path,
    environment: &mut HashMap<String, String>,
    gh_shim_ticket: Option<&str>,
) -> Result<(), String> {
    // The module uses these launch-identity variables to authenticate its own
    // daemon connection. Remove them only from the child snapshot so the module
    // process retains the credentials it needs.
    environment.retain(|key, _| !is_subc_credential_env_key(key));
    // This control belongs to the pinned config, never a caller or outer daemon.
    environment.remove(crate::privacy_spawn::CONTROL_ENV);
    if config.bash.disclaim_privacy {
        environment.insert(crate::privacy_spawn::CONTROL_ENV.to_owned(), "1".to_owned());
    }

    let gh_enabled = config.github.shim;
    environment.remove(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV);
    if gh_enabled {
        if let Some(ticket) = gh_shim_ticket {
            environment.insert(
                crate::gh_shim_ticket::GH_SHIM_TICKET_ENV.to_string(),
                ticket.to_string(),
            );
        }
    }
    let co_author_enabled = config.git.co_author != "off";

    // The inherited environment may already carry governance markers injected
    // by an OUTER daemon (agents spawn daemons in tests and tooling). Each
    // feature owns its markers in both directions: when disabled here, strip
    // what a parent injected so this process's children reflect THIS gate.
    // Only self-identifying values are removed - user-owned GIT_CONFIG_* is
    // untouched unless it provably points at an AFT-generated hooks dir.
    if !gh_enabled {
        if let Some(stale_shims) = environment.remove(GH_SHIMS_DIR_ENV) {
            if let Some(inherited) = environment.get("PATH").map(OsString::from) {
                let stale = PathBuf::from(&stale_shims);
                let cleaned: Vec<_> = std::env::split_paths(&inherited)
                    .filter(|entry| entry != &stale)
                    .collect();
                if let Ok(path) = std::env::join_paths(cleaned) {
                    environment.insert("PATH".to_string(), path.to_string_lossy().into_owned());
                }
            }
        }
    }
    if !co_author_enabled {
        environment.remove(GIT_CO_AUTHOR_ENV);
        environment.remove(GH_SHIM_BINARY_ENV);
        let aft_hooks_value = environment
            .get("GIT_CONFIG_VALUE_0")
            .is_some_and(|value| is_managed_git_hooks_path(Path::new(value)));
        if aft_hooks_value
            && environment.get("GIT_CONFIG_KEY_0").map(String::as_str) == Some("core.hooksPath")
        {
            environment.remove("GIT_CONFIG_COUNT");
            environment.remove("GIT_CONFIG_KEY_0");
            environment.remove("GIT_CONFIG_VALUE_0");
        }
    }
    if !gh_enabled && !co_author_enabled {
        return Ok(());
    }

    // Hooks and shims can invoke the AFT binary after the daemon's configure
    // request has completed. PROPAGATE an explicit storage override so those
    // child commands stay in the same storage universe - but never ORIGINATE
    // one: injecting the default-resolved shared root as an explicit env var
    // outranks XDG-based isolation in every nested process (field incident:
    // the daemon injected the real shared root into agent bash lanes, and 41
    // test-suite fixtures that isolate via HOME/XDG resolved the production
    // store). Children that resolve storage by default reach the same root
    // anyway; explicitness is only preserved, never minted.
    if let Some(explicit) = crate::environment::non_empty_os_var(STORAGE_DIR_ENV) {
        environment.insert(
            STORAGE_DIR_ENV.to_string(),
            explicit.to_string_lossy().into_owned(),
        );
    }
    // Preserve an explicitly selected gh-shim state directory for hooks and
    // nested AFT children, but never mint one from the operator's default.
    if let Some(explicit) = crate::environment::non_empty_os_var(GH_SHIM_STATE_DIR_ENV) {
        environment.insert(
            GH_SHIM_STATE_DIR_ENV.to_string(),
            explicit.to_string_lossy().into_owned(),
        );
    }
    maintain(config, storage_root)?;

    if gh_enabled {
        let shims_dir = storage_root.join(SHIMS_DIR_NAME);
        let inherited = environment
            .get("PATH")
            .map(OsString::from)
            .unwrap_or_else(|| crate::effective_path::effective_path().to_os_string());
        let mut entries = vec![shims_dir.clone()];
        entries.extend(std::env::split_paths(&inherited).filter(|entry| entry != &shims_dir));
        let path = std::env::join_paths(entries)
            .map_err(|error| format!("failed to construct governed child PATH: {error}"))?;
        environment.insert("PATH".to_string(), path.to_string_lossy().into_owned());
        environment.insert(
            GH_SHIMS_DIR_ENV.to_string(),
            shims_dir.to_string_lossy().into_owned(),
        );
    }

    if co_author_enabled {
        environment.insert("GIT_CONFIG_COUNT".to_string(), "1".to_string());
        environment.insert("GIT_CONFIG_KEY_0".to_string(), "core.hooksPath".to_string());
        environment.insert(
            "GIT_CONFIG_VALUE_0".to_string(),
            managed_git_hooks_dir(storage_root)
                .to_string_lossy()
                .into_owned(),
        );
        environment.insert(GIT_CO_AUTHOR_ENV.to_string(), config.git.co_author.clone());
        if config.git.co_author == "auto" {
            environment.insert(
                GH_SHIM_BINARY_ENV.to_string(),
                shim_binary(config)?.to_string_lossy().into_owned(),
            );
        }
    }

    Ok(())
}

pub fn shim_binary(config: &Config) -> Result<PathBuf, String> {
    let binary = match config.gh_shim.binary_path.as_ref() {
        Some(path) => path.clone(),
        None => std::env::current_exe()
            .map_err(|error| format!("failed to resolve the running AFT binary: {error}"))?,
    };
    if !binary.is_absolute() {
        return Err(format!(
            "gh_shim.binary_path must be absolute: {}",
            binary.display()
        ));
    }
    Ok(binary)
}

/// Refuse a shim candidate that lives inside the managed shims directory.
///
/// A pin pointing at the shims dir's own image is self-referential: maintain()
/// then always finds the link "consistent" with its candidate and the image
/// can only go stale — no version comparison can ever trigger a refresh. The
/// 2026-08-27 incident: a frozen Aug-25 copy refused the production-signed
/// manifest fleet-wide while every validity probe kept passing (a liveness
/// answer to a freshness question). Pins must reference a path something
/// external refreshes — the deploy path a placement updates, or no pin at all
/// so the running binary is the candidate.
fn reject_self_referential_pin(binary: &Path, shims_dir: &Path) -> Result<(), String> {
    let canonical_binary = binary
        .canonicalize()
        .unwrap_or_else(|_| binary.to_path_buf());
    let canonical_dir = shims_dir
        .canonicalize()
        .unwrap_or_else(|_| shims_dir.to_path_buf());
    if canonical_binary.starts_with(&canonical_dir) {
        return Err(format!(
            "gh_shim.binary_path points inside the managed shims directory ({}); a self-referential pin freezes the shim forever - point it at the deploy path a placement refreshes (e.g. ~/.local/share/cortexkit/bin/ck-aft) or remove it to track the running binary",
            binary.display()
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ShimProbeCacheKey {
    path: PathBuf,
    modified: Option<Duration>,
    size: u64,
}

#[derive(serde::Deserialize)]
struct ShimSelfReport {
    shim_version: String,
    gh_routing_schema_floor: u64,
}

static SHIM_PROBE_CACHE: OnceLock<Mutex<HashMap<ShimProbeCacheKey, Result<(), String>>>> =
    OnceLock::new();

/// Verify behavior rather than executable names: installation may point at a
/// renamed AFT image, while a process that merely resembles one must not become
/// the agent child's `gh` command.
fn probe_gh_shim_binary(binary: &Path) -> Result<(), String> {
    let metadata =
        fs::metadata(binary).map_err(|error| format!("could not stat candidate: {error}"))?;
    let key = ShimProbeCacheKey {
        path: binary.to_path_buf(),
        modified: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok()),
        size: metadata.len(),
    };
    let cache = SHIM_PROBE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned()
    {
        return cached;
    }

    let result = probe_gh_shim_binary_uncached(binary);
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, result.clone());
    result
}

fn probe_gh_shim_binary_uncached(binary: &Path) -> Result<(), String> {
    // Invoke the image directly, including on Windows where the managed entry is
    // a gh.cmd wrapper. This keeps validation independent of the wrapper's shell.
    let output = Command::new(binary)
        .args(["gh-shim", "--shim-version"])
        .output()
        .map_err(|error| format!("could not execute --shim-version probe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "--shim-version probe exited with {status}",
            status = output.status
        ));
    }
    let report: ShimSelfReport = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("--shim-version probe emitted invalid JSON: {error}"))?;
    if report.shim_version.is_empty() || report.gh_routing_schema_floor == 0 {
        return Err("--shim-version probe omitted required shim identity fields".to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn existing_gh_entry_is_valid(shims_dir: &Path) -> bool {
    let entry = shims_dir.join("gh");
    let binary = match fs::read_link(&entry) {
        Ok(target) if target.is_absolute() => target,
        Ok(target) => shims_dir.join(target),
        Err(_) => entry,
    };
    probe_gh_shim_binary(&binary).is_ok()
}

#[cfg(windows)]
fn existing_gh_entry_is_valid(shims_dir: &Path) -> bool {
    let entry = shims_dir.join("gh.cmd");
    let Ok(wrapper) = fs::read_to_string(entry) else {
        return false;
    };
    let Some(binary) = wrapper
        .strip_prefix("@echo off\r\n\"")
        .and_then(|line| line.strip_suffix("\" gh-shim %*\r\n"))
        .map(|path| PathBuf::from(path.replace("%%", "%")))
    else {
        return false;
    };
    probe_gh_shim_binary(&binary).is_ok()
}

#[cfg(not(any(unix, windows)))]
fn existing_gh_entry_is_valid(_shims_dir: &Path) -> bool {
    false
}

fn ensure_managed_git_hooks(hooks_dir: &Path) -> Result<(), String> {
    let expected = &managed_git_hook_set().hooks;
    if fs::symlink_metadata(hooks_dir).is_err() {
        install_managed_git_hook_set(hooks_dir, expected)?;
    }
    crate::private_storage::tighten_open_dir(hooks_dir.parent().unwrap_or(hooks_dir), hooks_dir);
    // The directory is shared by every storage root, so verify it before each
    // child launch: tampering in one place would otherwise reach every agent.
    // An intact set costs only reads here; nothing is rewritten or re-chmodded.
    quarantine_foreign_hook_entries(hooks_dir, expected)?;
    for (name, contents) in expected {
        write_hook_if_changed(&hooks_dir.join(name), contents.as_bytes())?;
    }
    if let Some(root) = hooks_dir.parent() {
        let report = refresh_legacy_git_hooks(root, Duration::from_secs(2))?;
        if report.bounded {
            // An unvisited directory may still contain the unguarded resolver.
            // Do not launch a child whose worktree chain could enter that code.
            return Err(format!(
                "AFT Git hooks refresh was bounded after {} cache entries; retry refresh or remove unused AFT hook cache directories in {}",
                report.entries_examined, root.display()
            ));
        }
    }
    Ok(())
}

const LEGACY_HOOK_DIRECTORY_SCAN_LIMIT: usize = 256;
const LEGACY_HOOK_MAX_BYTES: u64 = 64 * 1024;
const MANAGED_HOOK_HEADER: &[u8] = b"#!/bin/sh\n# AFT selects this hook through the agent child's environment. It does not alter\n# the repository or the user's Git configuration.\n";

#[derive(Debug, Default)]
struct LegacyHookRefresh {
    entries_examined: usize,
    hooks_examined: usize,
    rewritten: usize,
    bounded: bool,
}

/// Upgrade only recognizable AFT dispatchers in old content-keyed directories.
/// Their paths can outlive a version in a worktree manager's recorded hook chain;
/// leaving stale scripts there would make a current dispatcher reenter old code.
fn refresh_legacy_git_hooks(root: &Path, budget: Duration) -> Result<LegacyHookRefresh, String> {
    use std::io::Read;

    let deadline = Instant::now() + budget;
    let entries = fs::read_dir(root).map_err(|error| {
        format!(
            "failed to scan AFT Git hooks cache {}: {error}",
            root.display()
        )
    })?;
    let mut report = LegacyHookRefresh::default();
    // Bound enumeration itself, not a vector collected from an unbounded walk.
    for entry in entries.take(LEGACY_HOOK_DIRECTORY_SCAN_LIMIT) {
        if Instant::now() >= deadline {
            report.bounded = true;
            break;
        }
        report.entries_examined += 1;
        let entry =
            entry.map_err(|error| format!("failed to read AFT hooks cache entry: {error}"))?;
        let name = entry.file_name();
        let keyed = name.to_str().is_some_and(|name| {
            name.len() == GIT_HOOK_SET_KEY_LEN && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        if !keyed || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        for (hook_name, contents) in &managed_git_hook_set().hooks {
            if Instant::now() >= deadline {
                report.bounded = true;
                break;
            }
            let hook = entry.path().join(hook_name);
            let Ok(metadata) = fs::symlink_metadata(&hook) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            report.hooks_examined += 1;
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW);
            }
            let Ok(file) = options.open(&hook) else {
                continue;
            };
            let mut existing = Vec::new();
            file.take(LEGACY_HOOK_MAX_BYTES)
                .read_to_end(&mut existing)
                .map_err(|error| {
                    format!("failed to read old AFT hook {}: {error}", hook.display())
                })?;
            if !existing.starts_with(MANAGED_HOOK_HEADER) || existing == contents.as_bytes() {
                continue;
            }
            replace_legacy_hook(&hook, contents.as_bytes(), metadata.permissions())?;
            report.rewritten += 1;
            crate::slog_info!(
                "[agent_child_env] upgraded legacy AFT Git hook {}",
                hook.display()
            );
        }
        if report.bounded {
            break;
        }
    }
    report.bounded |= report.entries_examined == LEGACY_HOOK_DIRECTORY_SCAN_LIMIT;
    crate::slog_info!(
        "[agent_child_env] Git hooks refresh {}: entries_examined={} hooks_examined={} rewritten={} bounded={}",
        root.display(), report.entries_examined, report.hooks_examined, report.rewritten, report.bounded
    );
    Ok(report)
}

fn replace_legacy_hook(
    path: &Path,
    bytes: &[u8],
    permissions: fs::Permissions,
) -> Result<(), String> {
    use std::io::Write;
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        let _ = permissions;
        fs::Permissions::from_mode(crate::private_storage::DIR_MODE)
    };

    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = path.with_file_name(format!(
        ".{}.refresh-{}-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let replaced = (|| {
        let mut file = crate::private_storage::executable_options()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.set_permissions(permissions)?;
        drop(file);
        // rename replaces an existing regular file atomically, including on
        // Windows. Publish owner-only executables on Unix; retain Windows ACLs.
        fs::rename(&temporary, path)
    })();
    if let Err(error) = replaced {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "failed to refresh legacy AFT hook {}: {error}",
            path.display()
        ));
    }
    Ok(())
}

/// Create a complete hook set in one step: fill a private staging directory
/// beside the destination, then rename the whole directory into place. A
/// concurrent process (or test) therefore either sees no directory or a full
/// set of executable dispatchers, never a partly written one.
fn install_managed_git_hook_set(
    hooks_dir: &Path,
    expected: &[(&'static str, String)],
) -> Result<(), String> {
    let parent = hooks_dir.parent().ok_or_else(|| {
        format!(
            "child Git hooks directory has no parent: {}",
            hooks_dir.display()
        )
    })?;
    crate::private_storage::open_root(parent).map_err(|error| {
        format!(
            "failed to create child Git hooks directory {}: {error}",
            parent.display()
        )
    })?;
    // Threads of one process can install at once and the clock may not tick
    // between them, so a per-process counter keeps every staging name unique.
    static STAGING_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staging = parent.join(format!(
        ".staging-{}-{}-{}-{}",
        hooks_dir.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        STAGING_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let populated = (|| {
        crate::private_storage::create_dir(&staging).map_err(|error| {
            format!(
                "failed to create Git hooks staging directory {}: {error}",
                staging.display()
            )
        })?;
        for (name, contents) in expected {
            let hook = staging.join(name);
            crate::private_storage::write_executable(&hook, contents.as_bytes()).map_err(
                |error| {
                    format!(
                        "failed to write staged Git hook {}: {error}",
                        hook.display()
                    )
                },
            )?;
            #[cfg(unix)]
            set_executable(&hook)?;
        }
        Ok::<(), String>(())
    })();
    let installed = populated.and_then(|()| {
        fs::rename(&staging, hooks_dir).or_else(|error| {
            // Losing the race to another installer is success: the winner
            // renamed an equally complete set into place. Unix reports a
            // non-empty destination and Windows reports an existing one with
            // different error kinds, so check the destination instead.
            if hooks_dir.is_dir() {
                Ok(())
            } else {
                Err(format!(
                    "failed to install Git hooks directory {}: {error}",
                    hooks_dir.display()
                ))
            }
        })
    });
    if fs::symlink_metadata(&staging).is_ok() {
        let _ = fs::remove_dir_all(&staging);
    }
    installed
}

fn quarantine_foreign_hook_entries(
    hooks_dir: &Path,
    expected: &[(&'static str, String)],
) -> Result<(), String> {
    let mut foreign = Vec::new();
    for entry in fs::read_dir(hooks_dir).map_err(|error| {
        format!(
            "failed to inspect AFT-owned Git hooks directory {}: {error}",
            hooks_dir.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "failed to inspect an entry in AFT-owned Git hooks directory {}: {error}",
                hooks_dir.display()
            )
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let is_quarantine_dir = name == GIT_HOOKS_QUARANTINE_DIR_NAME
            && fs::symlink_metadata(&path).is_ok_and(|metadata| {
                metadata.file_type().is_dir() && !metadata.file_type().is_symlink()
            });
        if is_quarantine_dir {
            continue;
        }
        let expected_contents = name
            .to_str()
            .and_then(|name| expected.iter().find(|(expected, _)| *expected == name))
            .map(|(_, contents)| contents.as_bytes());
        let is_expected_file = expected_contents.is_some_and(|contents| {
            fs::symlink_metadata(&path).is_ok_and(|metadata| {
                metadata.file_type().is_file()
                    && !metadata.file_type().is_symlink()
                    && fs::read(&path).is_ok_and(|actual| actual == contents)
            })
        });
        if !is_expected_file {
            foreign.push(path);
        }
    }
    if foreign.is_empty() {
        return Ok(());
    }

    let quarantine = hooks_dir.join(GIT_HOOKS_QUARANTINE_DIR_NAME);
    let mut moved = Vec::new();
    if fs::symlink_metadata(&quarantine)
        .is_ok_and(|metadata| !metadata.file_type().is_dir() || metadata.file_type().is_symlink())
    {
        let staging = hooks_dir.join(format!(
            ".quarantine-stage-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        crate::private_storage::create_dir(&staging).map_err(|error| {
            format!(
                "failed to stage the Git hook quarantine directory {}: {error}",
                staging.display()
            )
        })?;
        let destination_name = quarantine_entry_name(&quarantine, 0);
        fs::rename(&quarantine, staging.join(&destination_name)).map_err(|error| {
            format!(
                "failed to quarantine reserved entry {}: {error}",
                quarantine.display()
            )
        })?;
        fs::rename(&staging, &quarantine).map_err(|error| {
            format!(
                "failed to install Git hook quarantine directory {}: {error}",
                quarantine.display()
            )
        })?;
        moved.push(quarantine.join(destination_name));
        foreign.retain(|path| path != &quarantine);
    } else {
        crate::private_storage::create_dir_all(&quarantine).map_err(|error| {
            format!(
                "failed to create Git hook quarantine directory {}: {error}",
                quarantine.display()
            )
        })?;
    }

    for (index, source) in foreign.into_iter().enumerate() {
        let destination = quarantine.join(quarantine_entry_name(&source, index + 1));
        match fs::rename(&source, &destination) {
            Ok(()) => moved.push(destination),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to quarantine foreign Git hook {} as {}: {error}",
                    source.display(),
                    destination.display()
                ));
            }
        }
    }
    if !moved.is_empty() {
        log_quarantined_hook_entries(hooks_dir, &moved);
    }
    Ok(())
}

fn quarantine_entry_name(source: &Path, index: usize) -> String {
    let original = source
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{timestamp}-{}-{index}-{original}", std::process::id())
}

fn log_quarantined_hook_entries(hooks_dir: &Path, moved: &[PathBuf]) {
    const WINDOW: Duration = Duration::from_secs(60);
    static LAST_WARNING: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    let now = Instant::now();
    let should_log = match LAST_WARNING
        .get_or_init(|| Mutex::new(HashMap::new()))
        .try_lock()
    {
        Ok(mut warnings) => {
            if warnings.len() > 512 {
                warnings.retain(|_, last| now.duration_since(*last) < WINDOW);
            }
            match warnings.get(hooks_dir) {
                Some(last) if now.duration_since(*last) < WINDOW => false,
                _ => {
                    warnings.insert(hooks_dir.to_path_buf(), now);
                    true
                }
            }
        }
        Err(_) => true,
    };
    if !should_log {
        return;
    }

    let destinations = moved
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!(
        "[agent_child_env] quarantined foreign content from AFT-owned Git hooks directory {}: {destinations}",
        hooks_dir.display()
    );
    crate::slog_warn!("{message}");
    #[cfg(test)]
    quarantine_test_logs().lock().unwrap().push(message);
}

#[cfg(test)]
fn quarantine_test_logs() -> &'static Mutex<Vec<String>> {
    static LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(unix)]
fn ensure_gh_entry(shims_dir: &Path, binary: &Path) -> Result<(), String> {
    use std::os::unix::fs::symlink;

    crate::private_storage::open_dir(shims_dir.parent().unwrap_or(shims_dir), shims_dir).map_err(
        |error| {
            format!(
                "failed to create gh shim directory {}: {error}",
                shims_dir.display()
            )
        },
    )?;
    let entry = shims_dir.join("gh");
    if fs::read_link(&entry).ok().as_deref() == Some(binary) {
        return Ok(());
    }
    if entry.is_dir() {
        return Err(format!(
            "cannot replace gh shim entry because it is a directory: {}",
            entry.display()
        ));
    }
    let temporary = shims_dir.join(format!(".gh.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    symlink(binary, &temporary).map_err(|error| {
        format!(
            "failed to create gh shim link {} -> {}: {error}",
            temporary.display(),
            binary.display()
        )
    })?;
    fs::rename(&temporary, &entry).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!(
            "failed to install gh shim link {}: {error}",
            entry.display()
        )
    })
}

#[cfg(windows)]
fn ensure_gh_entry(shims_dir: &Path, binary: &Path) -> Result<(), String> {
    crate::private_storage::create_dir_all(shims_dir).map_err(|error| {
        format!(
            "failed to create gh shim directory {}: {error}",
            shims_dir.display()
        )
    })?;
    write_if_changed(&shims_dir.join("gh.cmd"), &windows_gh_cmd(binary))
}

#[cfg(not(any(unix, windows)))]
fn ensure_gh_entry(_shims_dir: &Path, _binary: &Path) -> Result<(), String> {
    Err("gh child PATH injection is unsupported on this platform".to_string())
}

fn remove_gh_entry(shims_dir: &Path) -> Result<(), String> {
    for name in ["gh", "gh.cmd"] {
        let entry = shims_dir.join(name);
        match fs::remove_file(&entry) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to remove disabled gh shim entry {}: {error}",
                    entry.display()
                ));
            }
        }
    }
    match fs::remove_dir(shims_dir) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(format!(
            "failed to remove empty gh shim directory {}: {error}",
            shims_dir.display()
        )),
    }
}

/// Repair one dispatcher in an existing hook set. An intact hook is left
/// completely untouched (no write, no chmod), so a verified set never looks
/// like new content to a platform scanner. A replacement is made executable
/// before it is renamed into place, so git never finds a non-executable hook.
fn write_hook_if_changed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = fs::metadata(path)
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o777 == 0o700);
            if !executable {
                set_executable(path)?;
            }
        }
        return Ok(());
    }
    install_managed_file(path, bytes, true)
}

// Only the Windows `gh.cmd` wrapper is a plain managed file; hooks go through
// `write_hook_if_changed` so their executable bit is set before install.
#[cfg(windows)]
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    install_managed_file(path, bytes, false)
}

/// Replace one managed file through a temporary sibling and a rename, so a
/// reader sees either the old or the new bytes.
fn install_managed_file(path: &Path, bytes: &[u8], executable: bool) -> Result<(), String> {
    if path.is_dir() {
        return Err(format!(
            "cannot replace managed child file because it is a directory: {}",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("managed child file has no parent: {}", path.display()))?;
    crate::private_storage::create_dir_all(parent).map_err(|error| {
        format!(
            "failed to create managed child directory {}: {error}",
            parent.display()
        )
    })?;
    // The hook set is shared by every storage root, so two threads of one
    // process may repair the same file at once; the counter keeps their
    // temporary names apart.
    static TEMPORARY_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let write = if executable {
        crate::private_storage::write_executable(&temporary, bytes)
    } else {
        crate::private_storage::write(&temporary, bytes)
    };
    write.map_err(|error| {
        format!(
            "failed to write managed child file {}: {error}",
            temporary.display()
        )
    })?;
    #[cfg(unix)]
    if executable {
        set_executable(&temporary).inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    // Windows rename does not replace an existing destination. Managed files
    // contain no user data, so remove only the exact stale file before install.
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|error| {
            format!(
                "failed to replace stale managed child file {}: {error}",
                path.display()
            )
        })?;
    }
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!(
            "failed to install managed child file {}: {error}",
            path.display()
        )
    })
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            format!(
                "failed to open hook permissions {}: {error}",
                path.display()
            )
        })?;
    file.set_permissions(fs::Permissions::from_mode(crate::private_storage::DIR_MODE))
        .map_err(|error| format!("failed to make hook executable {}: {error}", path.display()))
}

/// Render the Windows command wrapper separately so its quoting contract can be
/// checked on every development platform; `cmd.exe` dispatch still requires
/// the native Windows CI oracle.
pub fn windows_gh_cmd(binary: &Path) -> Vec<u8> {
    let rendered = binary.to_string_lossy();
    debug_assert!(!rendered.contains('"'));
    let rendered = rendered.replace('%', "%%");
    format!("@echo off\r\n\"{rendered}\" gh-shim %*\r\n").into_bytes()
}

#[cfg(all(test, unix))]
pub(crate) fn write_storage_permission_fixture(root: &Path) {
    let hooks = root
        .join(GIT_HOOKS_DIR_NAME)
        .join(&managed_git_hook_set().key);
    ensure_managed_git_hooks(&hooks).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, GitConfig};

    fn test_thread_file(root: &Path, contents: Option<&[u8]>) {
        let path = root
            .parent()
            .unwrap()
            .join(".cargo")
            .join(ALFONSO_TEST_THREADS_FILE);
        if let Some(contents) = contents {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
    }

    #[test]
    fn worker_thread_budget_defaults_invalid_and_oversized_files() {
        let container = tempfile::tempdir().unwrap();
        let root = container.path().join("worktree");
        fs::create_dir(&root).unwrap();

        assert_eq!(worker_test_thread_budget(&root).0, 4);
        for contents in [
            b"\n".as_slice(),
            b"garbage\n",
            b"0\n",
            b"4",
            b"4\r\n",
            b"\xff\n",
        ] {
            test_thread_file(&root, Some(contents));
            assert_eq!(worker_test_thread_budget(&root).0, 4, "{contents:?}");
        }
        test_thread_file(&root, Some(b"9\n"));
        assert_eq!(worker_test_thread_budget(&root), (9, None));
        test_thread_file(&root, Some(b"256\n"));
        assert_eq!(worker_test_thread_budget(&root), (256, None));
        test_thread_file(&root, Some(b"257\n"));
        assert_eq!(
            worker_test_thread_budget(&root),
            (
                DEFAULT_WORKER_TEST_THREADS,
                Some(WorkerTestThreadBudgetIssue::Oversized("257".into()))
            )
        );
        test_thread_file(&root, Some(b"999999999999999999999999999999999\n"));
        assert_eq!(
            worker_test_thread_budget(&root).0,
            DEFAULT_WORKER_TEST_THREADS
        );

        let path = root
            .parent()
            .unwrap()
            .join(".cargo")
            .join(ALFONSO_TEST_THREADS_FILE);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(
            worker_test_thread_budget(&root).0,
            DEFAULT_WORKER_TEST_THREADS
        );
    }

    #[test]
    fn worker_thread_defaults_preserve_inherited_and_call_environment_values() {
        let container = tempfile::tempdir().unwrap();
        let root = container.path().join("worktree");
        fs::create_dir(&root).unwrap();
        test_thread_file(&root, Some(b"6\n"));

        let mut environment = HashMap::from([(NEXT_TEST_THREADS_ENV.into(), "13".into())]);
        inject_worker_test_threads_with(&root, &mut environment, |name| {
            name == RUST_TEST_THREADS_ENV
        });
        assert_eq!(environment[NEXT_TEST_THREADS_ENV], "13");
        assert!(!environment.contains_key(RUST_TEST_THREADS_ENV));

        let mut environment = HashMap::from([(RUST_TEST_THREADS_ENV.into(), "17".into())]);
        inject_worker_test_threads_with(&root, &mut environment, |_| false);
        assert_eq!(environment[RUST_TEST_THREADS_ENV], "17");
        assert_eq!(environment[NEXT_TEST_THREADS_ENV], "6");
    }

    #[cfg(unix)]
    const TEST_CO_AUTHOR: &str = "Pair Agent <pair@example.test>";

    #[test]
    fn disabled_features_leave_the_requested_environment_byte_identical() {
        let mut config = Config::default();
        config.github.shim = false;
        config.git = GitConfig::default();
        let before = HashMap::from([
            ("PATH".to_string(), "/one:/two".to_string()),
            ("CUSTOM".to_string(), "value".to_string()),
        ]);
        let mut after = before.clone();
        inject(&config, Path::new("/unused"), &mut after, None).unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn privacy_disclaim_control_comes_only_from_config_and_never_reaches_the_child() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        let mut environment =
            HashMap::from([(crate::privacy_spawn::CONTROL_ENV.to_owned(), "1".to_owned())]);
        inject(&config, root.path(), &mut environment, None).unwrap();
        assert!(!environment.contains_key(crate::privacy_spawn::CONTROL_ENV));
        config.bash.disclaim_privacy = true;
        inject(&config, root.path(), &mut environment, None).unwrap();
        assert!(crate::privacy_spawn::requested(&environment));
        let mut command = Command::new("sh");
        apply_to_command(&mut command, &environment);
        assert!(command
            .get_envs()
            .any(|(key, value)| key == crate::privacy_spawn::CONTROL_ENV && value.is_none()));
        let mut pty = portable_pty::CommandBuilder::new("sh");
        pty.env(crate::privacy_spawn::CONTROL_ENV, "1");
        scrub_pty_command(&mut pty);
        assert!(pty.get_env(crate::privacy_spawn::CONTROL_ENV).is_none());
    }

    #[test]
    fn gh_shim_ticket_is_set_only_from_the_argument_and_never_inherited() {
        let storage = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.github.shim = true;
        config.git = GitConfig::default();
        let ticket_key = crate::gh_shim_ticket::GH_SHIM_TICKET_ENV;

        // An inherited ticket belongs to some other command and is dropped.
        let mut inherited = HashMap::from([(ticket_key.to_string(), "stale".to_string())]);
        inject(&config, storage.path(), &mut inherited, None).unwrap();
        assert_eq!(inherited.get(ticket_key), None);

        let mut issued = HashMap::from([(ticket_key.to_string(), "stale".to_string())]);
        inject(&config, storage.path(), &mut issued, Some("fresh")).unwrap();
        assert_eq!(issued.get(ticket_key).map(String::as_str), Some("fresh"));
        // Only the ticket crosses into the child; no session id does.
        assert!(issued.keys().all(|key| !key.contains("SESSION")));

        // With the shim off, `gh` never reaches the shim, so no ticket is set.
        config.github.shim = false;
        let mut disabled = HashMap::new();
        inject(&config, storage.path(), &mut disabled, Some("fresh")).unwrap();
        assert_eq!(disabled.get(ticket_key), None);
    }

    #[test]
    fn child_environment_strips_the_complete_subc_credential_family_before_config_gates() {
        let mut config = Config::default();
        config.github.shim = false;
        config.git = GitConfig::default();
        let mut environment = HashMap::from([
            ("SUBC_MODULE_ID".to_string(), "aft".to_string()),
            ("SUBC_LAUNCH_NONCE".to_string(), "nonce".to_string()),
            (
                "SUBC_FUTURE_CREDENTIAL".to_string(),
                "future-secret".to_string(),
            ),
            ("CUSTOM".to_string(), "kept".to_string()),
        ]);

        inject(&config, Path::new("/unused"), &mut environment, None).unwrap();

        assert_eq!(environment.get("CUSTOM").map(String::as_str), Some("kept"));
        assert!(
            environment
                .keys()
                .all(|key| !is_subc_credential_env_key(key)),
            "a subc supervised-spawn credential remained in the child snapshot"
        );
    }

    #[test]
    fn command_adapters_remove_explicit_subc_identity_material() {
        let request_environment = HashMap::from([
            ("SUBC_MODULE_ID".to_string(), "request-aft".to_string()),
            ("CUSTOM".to_string(), "kept".to_string()),
        ]);
        let mut command = Command::new("unused-test-command");
        command
            .env("SUBC_LAUNCH_NONCE", "ambient-nonce")
            .env("SUBC_LAUNCH_NONCE_FD", "3:12345")
            .env("SUBC_FUTURE_CREDENTIAL", "future-secret");
        apply_to_command(&mut command, &request_environment);
        let configured = command
            .get_envs()
            .map(|(key, value)| (key.to_string_lossy().into_owned(), value))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            configured.get("CUSTOM").copied().flatten(),
            Some(std::ffi::OsStr::new("kept"))
        );
        for key in [
            "SUBC_MODULE_ID",
            "SUBC_LAUNCH_NONCE",
            "SUBC_LAUNCH_NONCE_FD",
            "SUBC_FUTURE_CREDENTIAL",
        ] {
            assert_eq!(
                configured.get(key).copied().flatten(),
                None,
                "std::process child retained {key}"
            );
        }

        let mut pty_command = portable_pty::CommandBuilder::new("unused-test-command");
        pty_command.env("SUBC_MODULE_ID", "aft");
        pty_command.env("SUBC_LAUNCH_NONCE", "nonce");
        pty_command.env("SUBC_LAUNCH_NONCE_FD", "3:12345");
        pty_command.env("SUBC_FUTURE_CREDENTIAL", "future-secret");
        pty_command.env("CUSTOM", "kept");
        scrub_pty_command(&mut pty_command);
        assert_eq!(
            pty_command.get_env("CUSTOM"),
            Some(std::ffi::OsStr::new("kept"))
        );
        for key in [
            "SUBC_MODULE_ID",
            "SUBC_LAUNCH_NONCE",
            "SUBC_LAUNCH_NONCE_FD",
            "SUBC_FUTURE_CREDENTIAL",
        ] {
            assert_eq!(pty_command.get_env(key), None, "PTY child retained {key}");
        }
    }

    #[test]
    fn agent_process_creation_sites_cannot_bypass_the_child_environment_funnel() {
        // Normalize line endings first: Windows checkouts materialize these
        // sources with CRLF, and a split marker containing a bare \n would
        // silently never match there - leaving the test half in the counted
        // text and failing the inventory with test-code spawn sites.
        let registry_source = include_str!("bash_background/registry.rs").replace("\r\n", "\n");
        let registry = registry_source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        let pty_source = include_str!("bash_background/pty_process.rs").replace("\r\n", "\n");
        let pty = pty_source
            .split("// Every test in this module")
            .next()
            .unwrap();
        let sandbox = include_str!("sandbox_spawn.rs");

        let detached_spawns = registry.matches(".spawn()").count();
        assert_eq!(detached_spawns, 2, "agent detached spawn inventory drifted");
        assert_eq!(
            registry
                .matches("agent_child_env::apply_to_command")
                .count(),
            detached_spawns,
            "every detached spawn must apply the scrubbed child environment"
        );

        let pty_spawns = pty.matches(".spawn_command(").count();
        // The macOS opt-in branch uses posix_spawn; these two inherited
        // alternatives are mutually exclusive cfg paths.
        assert_eq!(pty_spawns, 2, "agent PTY spawn inventory drifted");
        assert_eq!(
            pty.matches("sandbox_spawn::pty_command_for_plan(").count(),
            1,
            "every PTY spawn must use the scrubbed command factory"
        );
        assert!(
            sandbox.contains("agent_child_env::scrub_pty_command(&mut command)"),
            "the PTY command factory no longer scrubs child credentials"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configure_maintenance_refreshes_stale_gh_links_and_removes_disabled_entries() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("aft-first");
        let second = temp.path().join("aft-second");
        write_self_reporting_shim(&first);
        write_self_reporting_shim(&second);
        let mut config = Config::default();
        config.gh_shim.binary_path = Some(first);
        maintain(&config, temp.path()).unwrap();
        let entry = temp.path().join("shims/gh");
        assert_eq!(
            fs::read_link(&entry).unwrap(),
            config.gh_shim.binary_path.as_deref().unwrap()
        );

        config.gh_shim.binary_path = Some(second);
        maintain(&config, temp.path()).unwrap();
        assert_eq!(
            fs::read_link(&entry).unwrap(),
            config.gh_shim.binary_path.as_deref().unwrap()
        );

        config.github.shim = false;
        maintain(&config, temp.path()).unwrap();
        assert!(fs::symlink_metadata(entry).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn configure_maintenance_refuses_harnesses_and_preserves_verified_shims() {
        let temp = tempfile::tempdir().unwrap();
        let verified = temp.path().join("aft-verified");
        let harness = temp.path().join("aft-test-harness");
        write_self_reporting_shim(&verified);
        write_executable(
            &harness,
            "#!/bin/sh\nif [ \"${2:-}\" = \"--shim-version\" ]; then exit 2; fi\nexit 0\n",
        );

        let mut config = Config::default();
        config.gh_shim.binary_path = Some(verified.clone());
        maintain(&config, temp.path()).unwrap();
        let entry = temp.path().join("shims/gh");
        assert_eq!(fs::read_link(&entry).unwrap(), verified);

        config.gh_shim.binary_path = Some(harness);
        maintain(&config, temp.path()).unwrap();
        assert_eq!(
            fs::read_link(&entry).unwrap(),
            verified,
            "a rejected candidate must not replace a verified shim"
        );

        fs::remove_file(&entry).unwrap();
        maintain(&config, temp.path()).unwrap();
        assert!(
            fs::symlink_metadata(entry).is_err(),
            "a rejected candidate must not install a new gh entry"
        );
    }

    #[test]
    fn windows_wrapper_uses_the_explicit_gh_shim_dispatch_form() {
        assert_eq!(
            String::from_utf8(windows_gh_cmd(Path::new(r"C:\AFT Dev\aft.exe"))).unwrap(),
            "@echo off\r\n\"C:\\AFT Dev\\aft.exe\" gh-shim %*\r\n"
        );
    }

    #[test]
    fn generated_hook_stays_posix_and_documents_joint_agent_attribution() {
        let hook = managed_git_hook_contents(PREPARE_COMMIT_MSG);
        assert!(hook.starts_with("#!/bin/sh\n"));
        assert!(!hook.contains("[["));
        assert!(!hook.contains("function "));
        assert!(!hook.contains("mason:*)"));
        assert!(hook.contains("do not\n# receive an attribution exemption"));
        assert!(hook.contains("git interpret-trailers --in-place --if-exists doNothing"));
        assert!(hook.contains("--trailer \"Co-authored-by=$identity\" \"$msg_file\""));
        assert!(!hook.contains(">> \"$msg_file\""));
    }

    #[cfg(unix)]
    fn run_git(repo: &Path, args: &[&str], environment: &HashMap<String, String>) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .envs(environment)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed: {status}");
    }

    // The timeout turns a dispatcher that re-enters itself (an infinite hook
    // loop) into a failure; it is not a bound on commit latency. A hook-chained
    // commit spawns several git and shell processes, which under a parallel test
    // gate on macOS can take multiple seconds each, so keep it far above that.
    #[cfg(unix)]
    const HOOK_REENTRY_GUARD: Duration = Duration::from_secs(60);

    #[cfg(unix)]
    fn run_git_with_timeout(
        repo: &Path,
        args: &[&str],
        environment: &HashMap<String, String>,
        timeout: Duration,
    ) -> std::process::Output {
        use std::process::Stdio;

        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(repo)
            .envs(environment)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::bash_background::process::start_new_session(&mut command);
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                crate::bash_background::process::terminate_process(&mut child);
                let output = child.wait_with_output().unwrap();
                panic!(
                    "git {args:?} exceeded {timeout:?}; stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn initialize_repo(repo: &Path) {
        fs::create_dir_all(repo).unwrap();
        let environment = HashMap::new();
        run_git(repo, &["init", "--quiet"], &environment);
        run_git(repo, &["config", "user.name", "AFT Test"], &environment);
        run_git(
            repo,
            &["config", "user.email", "aft-test@example.test"],
            &environment,
        );
        fs::write(repo.join("tracked.txt"), "one\n").unwrap();
        run_git(repo, &["add", "tracked.txt"], &environment);
    }

    #[cfg(unix)]
    fn prepare_merge_fixture(repo: &Path) {
        let environment = HashMap::new();
        initialize_repo(repo);
        run_git(repo, &["commit", "--quiet", "-m", "initial"], &environment);
        run_git(repo, &["checkout", "--quiet", "-b", "topic"], &environment);
        fs::write(repo.join("topic.txt"), "topic\n").unwrap();
        run_git(repo, &["add", "topic.txt"], &environment);
        run_git(repo, &["commit", "--quiet", "-m", "topic"], &environment);
        run_git(repo, &["checkout", "--quiet", "-"], &environment);
    }

    #[cfg(unix)]
    fn commit_message(repo: &Path) -> String {
        let output = std::process::Command::new("git")
            .args(["cat-file", "commit", "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .split_once("\n\n")
            .unwrap()
            .1
            .to_string()
    }

    #[cfg(unix)]
    fn co_author_environment(storage: &Path) -> HashMap<String, String> {
        let mut config = Config::default();
        config.github.shim = false;
        config.git.co_author = TEST_CO_AUTHOR.to_string();
        let mut environment = HashMap::new();
        inject(&config, storage, &mut environment, None).unwrap();
        environment
    }

    #[cfg(unix)]
    fn expected_co_author_message(subject: &str) -> String {
        format!("{subject}\n\nCo-authored-by: {TEST_CO_AUTHOR}\n")
    }

    #[cfg(unix)]
    fn assert_single_co_author_message(message: &str, subject: &str) {
        assert_eq!(message, expected_co_author_message(subject));
        assert_eq!(message.matches("Co-authored-by:").count(), 1);
    }

    #[cfg(unix)]
    fn run_generated_hook(
        repo: &Path,
        hook: &Path,
        message_file: &Path,
        environment: &HashMap<String, String>,
    ) {
        let status = std::process::Command::new(hook)
            .arg(message_file)
            .current_dir(repo)
            .envs(environment)
            .status()
            .unwrap();
        assert!(status.success(), "generated hook failed: {status}");
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        set_executable(path).unwrap();
    }

    #[cfg(unix)]
    fn write_self_reporting_shim(path: &Path) {
        write_executable(
            path,
            "#!/bin/sh\nif [ \"${1:-}\" = \"gh-shim\" ] && [ \"${2:-}\" = \"--shim-version\" ]; then\n  printf '%s\\n' '{\"shim_version\":\"test\",\"gh_routing_schema_floor\":1}'\n  exit 0\nfi\nexit 1\n",
        );
    }

    #[test]
    fn child_environment_propagates_explicit_storage_override_but_never_originates_one() {
        let _guard = crate::test_env::process_env_lock();
        let storage = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.git.co_author = "Pair Agent <pair@example.test>".to_string();

        // No explicit override in the parent: the child gets NONE. Injecting the
        // default-resolved root as an explicit env var would outrank XDG-based
        // isolation in nested processes (the 41-fixture field incident).
        let previous = std::env::var_os(STORAGE_DIR_ENV);
        std::env::remove_var(STORAGE_DIR_ENV);
        let mut environment = HashMap::new();
        inject(&config, storage.path(), &mut environment, None).unwrap();
        assert_eq!(environment.get(STORAGE_DIR_ENV), None);

        // Explicit override present: propagated verbatim so spawned children
        // stay in the same storage universe (the original leak-class fix).
        let explicit = tempfile::tempdir().unwrap();
        std::env::set_var(STORAGE_DIR_ENV, explicit.path());
        let mut environment = HashMap::new();
        inject(&config, storage.path(), &mut environment, None).unwrap();
        assert_eq!(
            environment.get(STORAGE_DIR_ENV),
            Some(&explicit.path().to_string_lossy().into_owned())
        );
        match previous {
            Some(value) => std::env::set_var(STORAGE_DIR_ENV, value),
            None => std::env::remove_var(STORAGE_DIR_ENV),
        }
    }

    #[cfg(unix)]
    #[test]
    fn generated_hook_separates_a_merge_subject_without_a_final_newline() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        prepare_merge_fixture(&repo);

        let environment = co_author_environment(&storage);
        run_git(
            &repo,
            &[
                "merge",
                "--no-ff",
                "--quiet",
                "-m",
                "merge subject",
                "topic",
            ],
            &environment,
        );

        assert_single_co_author_message(&commit_message(&repo), "merge subject");
    }

    #[cfg(unix)]
    #[test]
    fn generated_hook_keeps_plain_commit_m_messages_in_trailer_form() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);

        let environment = co_author_environment(&storage);
        run_git(
            &repo,
            &["commit", "--quiet", "-m", "plain subject"],
            &environment,
        );

        assert_single_co_author_message(&commit_message(&repo), "plain subject");
    }

    #[cfg(unix)]
    #[test]
    fn generated_hook_does_not_duplicate_a_trailer_when_rerun() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        let environment = co_author_environment(&storage);
        let hook = managed_git_hooks_dir(&storage).join(PREPARE_COMMIT_MSG);
        let message_file = repo.join("message");
        fs::write(&message_file, "rerun subject").unwrap();

        run_generated_hook(&repo, &hook, &message_file, &environment);
        run_generated_hook(&repo, &hook, &message_file, &environment);

        assert_single_co_author_message(
            &fs::read_to_string(message_file).unwrap(),
            "rerun subject",
        );
    }

    #[cfg(unix)]
    #[test]
    fn generated_hook_does_nothing_when_another_co_author_exists() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        let environment = co_author_environment(&storage);
        let hook = managed_git_hooks_dir(&storage).join(PREPARE_COMMIT_MSG);
        let message_file = repo.join("message");
        let original = "existing subject\n\nCo-authored-by: Other Agent <other@example.test>\n";
        fs::write(&message_file, original).unwrap();

        run_generated_hook(&repo, &hook, &message_file, &environment);

        assert_eq!(fs::read_to_string(message_file).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn generated_hook_and_chained_sibling_add_only_one_matching_trailer() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        prepare_merge_fixture(&repo);
        let local_hook = repo.join(".git/hooks/prepare-commit-msg");
        write_executable(
            &local_hook,
            "#!/bin/sh\nprintf '%s\\n' invoked > sibling-hook-ran\ngit interpret-trailers --in-place --if-exists doNothing --trailer \"Co-authored-by=Pair Agent <pair@example.test>\" \"$1\"\n",
        );

        let environment = co_author_environment(&storage);
        run_git(
            &repo,
            &[
                "merge",
                "--no-ff",
                "--quiet",
                "-m",
                "chained merge subject",
                "topic",
            ],
            &environment,
        );

        assert_eq!(
            fs::read_to_string(repo.join("sibling-hook-ran")).unwrap(),
            "invoked\n"
        );
        assert_single_co_author_message(&commit_message(&repo), "chained merge subject");
    }

    #[cfg(unix)]
    #[test]
    fn auto_hook_is_idempotent_and_chains_default_repository_hook() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        let shim = temp.path().join("fake-aft");
        write_executable(
            &shim,
            "#!/bin/sh\nprintf '%s\\n' 'Co-authored-by: aft-alfonso[bot] <318960130+aft-alfonso[bot]@users.noreply.github.com>'\n",
        );
        let local_hook = repo.join(".git/hooks/prepare-commit-msg");
        write_executable(
            &local_hook,
            "#!/bin/sh\nprintf '%s\\n' 'Local-Hook: default' >> \"$1\"\n",
        );

        let mut config = Config::default();
        config.github.shim = false;
        config.gh_shim.binary_path = Some(shim);
        config.git.co_author = "auto".to_string();
        let mut environment = HashMap::new();
        inject(&config, &storage, &mut environment, None).unwrap();
        run_git(
            &repo,
            &["commit", "--quiet", "-m", "mason: joint work"],
            &environment,
        );
        run_git(
            &repo,
            &["commit", "--quiet", "--amend", "--no-edit"],
            &environment,
        );

        let message = commit_message(&repo);
        assert_eq!(message.matches("Co-authored-by:").count(), 1);
        assert!(message.contains(
            "Co-authored-by: aft-alfonso[bot] <318960130+aft-alfonso[bot]@users.noreply.github.com>"
        ));
        assert_eq!(message.matches("Local-Hook: default").count(), 2);
    }

    /// Every storage root (every test, every project) must share one hook set.
    /// macOS scans each newly created executable on its first run, so a fresh
    /// copy per storage root costs a security scan per hook per root.
    #[cfg(unix)]
    #[test]
    fn separate_storage_roots_share_one_hook_set_without_rewriting_it() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let first_storage = temp.path().join("first-storage");
        let second_storage = temp.path().join("second-storage");
        let first = co_author_environment(&first_storage);
        let first_dir = PathBuf::from(first.get("GIT_CONFIG_VALUE_0").unwrap());
        let snapshot = |dir: &Path| {
            // A quarantine directory is AFT's own and may persist in the shared
            // set from an earlier repair; only the hook files matter here.
            let mut entries = fs::read_dir(dir)
                .unwrap()
                .filter(|entry| {
                    entry.as_ref().unwrap().file_name() != GIT_HOOKS_QUARANTINE_DIR_NAME
                })
                .map(|entry| {
                    let entry = entry.unwrap();
                    let metadata = fs::symlink_metadata(entry.path()).unwrap();
                    (
                        entry.file_name(),
                        metadata.ino(),
                        metadata.mtime(),
                        metadata.mtime_nsec(),
                        metadata.ctime(),
                        metadata.ctime_nsec(),
                    )
                })
                .collect::<Vec<_>>();
            entries.sort();
            entries
        };
        let before = snapshot(&first_dir);

        let second = co_author_environment(&second_storage);
        let second_dir = PathBuf::from(second.get("GIT_CONFIG_VALUE_0").unwrap());

        assert_eq!(
            second_dir, first_dir,
            "two storage roots selected different hook directories"
        );
        assert!(
            !first_dir.starts_with(&first_storage) && !second_dir.starts_with(&second_storage),
            "the shared hook set must live outside every storage root: {}",
            first_dir.display()
        );
        assert_eq!(before.len(), MANAGED_GIT_HOOK_NAMES.len());
        assert_eq!(
            snapshot(&second_dir),
            before,
            "the second install created, replaced, or touched a hook file"
        );
    }

    #[test]
    fn hook_set_root_follows_the_aft_cache_root_and_ignores_relative_values() {
        let storage = Path::new("/storage");
        let lookup = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let root = |pairs, windows| managed_git_hooks_root_from(lookup(pairs), windows, storage);

        #[cfg(unix)]
        {
            assert_eq!(
                root(
                    &[("AFT_CACHE_DIR", "/explicit"), ("HOME", "/home/u")],
                    false
                ),
                Path::new("/explicit/git-hooks")
            );
            assert_eq!(
                root(&[("XDG_CACHE_HOME", "/xdg"), ("HOME", "/home/u")], false),
                Path::new("/xdg/aft/git-hooks")
            );
            assert_eq!(
                root(
                    &[("XDG_CACHE_HOME", "relative"), ("HOME", "/home/u")],
                    false
                ),
                Path::new("/home/u/.cache/aft/git-hooks")
            );
        }
        assert_eq!(root(&[], false), Path::new("/storage/git-hooks"));
        #[cfg(windows)]
        {
            assert_eq!(
                root(&[("LOCALAPPDATA", r"C:\Users\u\AppData\Local")], true),
                Path::new(r"C:\Users\u\AppData\Local\aft\git-hooks")
            );
            assert_eq!(
                root(&[("USERPROFILE", r"C:\Users\u")], true),
                Path::new(r"C:\Users\u\AppData\Local\aft\git-hooks")
            );
        }
    }

    #[test]
    fn managed_hooks_path_recognition_covers_keyed_and_legacy_directories() {
        let key = &managed_git_hook_set().key;
        assert_eq!(key.len(), GIT_HOOK_SET_KEY_LEN);
        assert!(is_managed_git_hooks_path(
            &Path::new("/c/aft").join(GIT_HOOKS_DIR_NAME).join(key)
        ));
        assert!(is_managed_git_hooks_path(
            &Path::new("/s").join(GIT_HOOKS_DIR_NAME)
        ));
        assert!(!is_managed_git_hooks_path(
            &Path::new("/repo").join(GIT_HOOKS_DIR_NAME).join("custom")
        ));
        assert!(!is_managed_git_hooks_path(
            &Path::new("/repo/.githooks").join(key)
        ));
    }

    #[test]
    fn disabled_attribution_strips_an_inherited_keyed_hooks_path() {
        let mut config = Config::default();
        config.github.shim = false;
        config.git = GitConfig::default();
        let keyed = Path::new("/c/aft")
            .join(GIT_HOOKS_DIR_NAME)
            .join(&managed_git_hook_set().key);
        let mut environment = HashMap::from([
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            ("GIT_CONFIG_KEY_0".to_string(), "core.hooksPath".to_string()),
            (
                "GIT_CONFIG_VALUE_0".to_string(),
                keyed.to_string_lossy().into_owned(),
            ),
        ]);
        inject(&config, Path::new("/unused"), &mut environment, None).unwrap();
        assert!(environment.is_empty(), "{environment:?}");
    }

    /// Many processes and tests install the same shared set at once; each must
    /// succeed and the result must be one complete, executable set with no
    /// staging leftovers.
    #[test]
    fn concurrent_installers_produce_one_complete_hook_set() {
        let temp = tempfile::tempdir().unwrap();
        let hooks_dir = temp
            .path()
            .join(GIT_HOOKS_DIR_NAME)
            .join(&managed_git_hook_set().key);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let workers = (0..4)
            .map(|_| {
                let hooks_dir = hooks_dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    ensure_managed_git_hooks(&hooks_dir)
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }

        for (name, contents) in &managed_git_hook_set().hooks {
            assert_eq!(fs::read_to_string(hooks_dir.join(name)).unwrap(), *contents);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(hooks_dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o700, "{name} is not owner-only executable");
            }
        }
        assert_eq!(
            fs::read_dir(&hooks_dir).unwrap().count(),
            MANAGED_GIT_HOOK_NAMES.len()
        );
        let siblings = fs::read_dir(hooks_dir.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            siblings.len(),
            1,
            "staging directories were left behind: {siblings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_generates_the_complete_posix_dispatcher_set() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let mut config = Config::default();
        config.github.shim = false;
        config.git.co_author = TEST_CO_AUTHOR.to_string();

        maintain(&config, &storage).unwrap();

        let hooks_dir = managed_git_hooks_dir(&storage);
        // The set is shared per user, so an earlier repair may have left AFT's
        // own quarantine directory beside the dispatchers.
        let mut generated = fs::read_dir(&hooks_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != GIT_HOOKS_QUARANTINE_DIR_NAME)
            .collect::<Vec<_>>();
        generated.sort();
        let mut expected = MANAGED_GIT_HOOK_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(generated, expected);
        for name in MANAGED_GIT_HOOK_NAMES {
            let body = fs::read_to_string(hooks_dir.join(name)).unwrap();
            assert!(
                body.starts_with("#!/bin/sh\n"),
                "{name} is not a POSIX hook"
            );
            assert!(body.contains(&format!("hook_name={name}\n")));
            assert!(!body.lines().any(|line| {
                !line.trim_start().starts_with('#') && line.contains("rev-parse --git-path")
            }));
            assert!(body.contains("rev-parse --git-dir"));
            assert!(body.contains("-ef \"$0\""));
            let status = Command::new("/bin/sh")
                .arg("-n")
                .arg(hooks_dir.join(name))
                .status()
                .unwrap();
            assert!(status.success(), "{name} is not valid POSIX shell syntax");
        }
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_quarantines_contamination_logs_and_regenerates() {
        let temp = tempfile::tempdir().unwrap();
        // Contaminating the shared per-user set would disturb concurrently
        // running tests, so this exercises a private copy of the same layout.
        let hooks_dir = temp
            .path()
            .join(GIT_HOOKS_DIR_NAME)
            .join(&managed_git_hook_set().key);
        ensure_managed_git_hooks(&hooks_dir).unwrap();
        fs::write(
            hooks_dir.join("pre-commit"),
            "#!/bin/sh\necho foreign lefthook fallback\n",
        )
        .unwrap();
        fs::write(hooks_dir.join("unknown-manager-hook"), "foreign\n").unwrap();

        ensure_managed_git_hooks(&hooks_dir).unwrap();

        assert_eq!(
            fs::read_to_string(hooks_dir.join("pre-commit")).unwrap(),
            managed_git_hook_contents("pre-commit")
        );
        let quarantined = fs::read_dir(hooks_dir.join(GIT_HOOKS_QUARANTINE_DIR_NAME))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(quarantined.len(), 2);
        assert!(quarantined.iter().any(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("pre-commit")
                && fs::read_to_string(path)
                    .unwrap()
                    .contains("foreign lefthook fallback")
        }));
        assert!(quarantined.iter().any(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("unknown-manager-hook")
        }));
        let hook_dir_text = hooks_dir.display().to_string();
        let warning_count = quarantine_test_logs()
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.contains(&hook_dir_text))
            .count();
        assert_eq!(warning_count, 1, "the contamination sweep did not log once");

        fs::write(hooks_dir.join("another-foreign-hook"), "foreign again\n").unwrap();
        ensure_managed_git_hooks(&hooks_dir).unwrap();
        let warning_count = quarantine_test_logs()
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.contains(&hook_dir_text))
            .count();
        assert_eq!(
            warning_count, 1,
            "quarantine warnings were not rate-limited"
        );
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_content_guard_detects_a_one_byte_managed_hook_mutation() {
        let temp = tempfile::tempdir().unwrap();
        // Contaminating the shared per-user set would disturb concurrently
        // running tests, so this exercises a private copy of the same layout.
        let hooks_dir = temp
            .path()
            .join(GIT_HOOKS_DIR_NAME)
            .join(&managed_git_hook_set().key);
        ensure_managed_git_hooks(&hooks_dir).unwrap();
        assert!(!hooks_dir.join(GIT_HOOKS_QUARANTINE_DIR_NAME).exists());

        let hook = hooks_dir.join("commit-msg");
        let mut mutated = fs::read(&hook).unwrap();
        mutated.push(b' ');
        fs::write(&hook, mutated).unwrap();
        ensure_managed_git_hooks(&hooks_dir).unwrap();

        let quarantine = hooks_dir.join(GIT_HOOKS_QUARANTINE_DIR_NAME);
        assert_eq!(fs::read_dir(quarantine).unwrap().count(), 1);
        assert_eq!(
            fs::read_to_string(hook).unwrap(),
            managed_git_hook_contents("commit-msg")
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_hooks_path_without_a_hook_does_not_reenter_the_dispatcher() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        run_git(
            &repo,
            &["config", "core.hooksPath", ".githooks"],
            &HashMap::new(),
        );
        fs::create_dir_all(repo.join(".githooks")).unwrap();
        let environment = co_author_environment(&storage);

        let output = run_git_with_timeout(
            &repo,
            &["commit", "--quiet", "-m", "no repository hook"],
            &environment,
            HOOK_REENTRY_GUARD,
        );

        assert!(
            output.status.success(),
            "commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_hooks_path_pointing_to_managed_directory_does_not_reenter() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        let environment = co_author_environment(&storage);
        let managed = managed_git_hooks_dir(&storage);
        run_git(
            &repo,
            &["config", "core.hooksPath", managed.to_str().unwrap()],
            &HashMap::new(),
        );

        let output = run_git_with_timeout(
            &repo,
            &["commit", "--quiet", "-m", "self guard"],
            &environment,
            HOOK_REENTRY_GUARD,
        );

        assert!(
            output.status.success(),
            "commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn worktree_hooks_chaining_back_to_dispatcher_finish_add_and_commit() {
        assert_worktree_hook_chain(false);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_dispatchers_are_refreshed_before_worktree_add_and_commit() {
        assert_worktree_hook_chain(true);
    }

    #[cfg(unix)]
    fn legacy_dispatcher(hook: &str) -> String {
        // The unguarded resolver/exec shape that worktree managers captured in
        // older AFT versions. Keep it independent of the current template.
        format!("{}hook_name={hook}\nrepo_root=$(git rev-parse --show-toplevel 2>/dev/null || :)\nfor scope in --worktree --local --global --system; do\n  configured_hooks=$(git config \"$scope\" --get core.hooksPath 2>/dev/null || :)\n  if [ -n \"$configured_hooks\" ]; then\n    candidate=\"$configured_hooks/$hook_name\"\n    if [ -x \"$candidate\" ] && ! [ \"$candidate\" -ef \"$0\" ]; then exec \"$candidate\" \"$@\"; fi\n    exit 0\n  fi\ndone\nexit 0\n", std::str::from_utf8(MANAGED_HOOK_HEADER).unwrap())
    }

    #[cfg(unix)]
    fn assert_worktree_hook_chain(legacy: bool) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        initialize_repo(&repo);
        run_git(
            &repo,
            &["commit", "--allow-empty", "-m", "initial"],
            &HashMap::new(),
        );
        run_git(
            &repo,
            &["config", "extensions.worktreeConfig", "true"],
            &HashMap::new(),
        );
        let worktree = temp.path().join("linked");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "worker",
                worktree.to_str().unwrap(),
            ],
            &HashMap::new(),
        );
        let cache = temp.path().join("git-hooks");
        let managed = cache.join(&managed_git_hook_set().key);
        let original = if legacy {
            cache.join("11111111111111111111111111111111")
        } else {
            managed.clone()
        };
        if legacy {
            fs::create_dir_all(&original).unwrap();
            for hook in ["post-index-change", PREPARE_COMMIT_MSG] {
                write_executable(&original.join(hook), &legacy_dispatcher(hook));
            }
        }
        ensure_managed_git_hooks(&managed).unwrap();
        let environment = HashMap::from([
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "core.hooksPath".into()),
            (
                "GIT_CONFIG_VALUE_0".into(),
                managed.to_string_lossy().into_owned(),
            ),
            (GIT_CO_AUTHOR_ENV.into(), TEST_CO_AUTHOR.into()),
        ]);
        let hooks = temp.path().join("mason-hooks");
        fs::create_dir(&hooks).unwrap();
        let calls = temp.path().join("calls");
        for hook in ["post-index-change", PREPARE_COMMIT_MSG] {
            // A worktree manager can record the ambient command-scope path as
            // its original hooks directory, then chain back to the dispatcher.
            write_executable(&hooks.join(hook), &format!(
                "#!/bin/sh\nprintf '%s\\n' '{hook}' >> '{}'\nhook='{}'\nif test -x \"$hook\"; then exec \"$hook\" \"$@\"; fi\nexit 0\n",
                calls.display(), original.join(hook).display()
            ));
            write_executable(
                &repo.join(".git/hooks").join(hook),
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' 'native-{hook}' >> '{}'\nexec '{}' \"$@\"\n",
                    calls.display(),
                    original.join(hook).display()
                ),
            );
        }
        run_git(
            &worktree,
            &[
                "config",
                "--worktree",
                "core.hooksPath",
                hooks.to_str().unwrap(),
            ],
            &HashMap::new(),
        );
        fs::write(worktree.join("data"), "delivery\n").unwrap();
        for args in [&["add", "-A"][..], &["commit", "-m", "delivery"][..]] {
            let output = run_git_with_timeout(&worktree, args, &environment, HOOK_REENTRY_GUARD);
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_single_co_author_message(&commit_message(&worktree), "delivery");
        let calls = fs::read_to_string(calls).unwrap();
        assert!(calls.lines().any(|line| line == "post-index-change"));
        assert_eq!(
            calls
                .lines()
                .filter(|line| *line == PREPARE_COMMIT_MSG)
                .count(),
            1
        );
        assert_eq!(
            calls
                .lines()
                .filter(|line| *line == "native-prepare-commit-msg")
                .count(),
            1
        );
        assert!(calls.lines().any(|line| line == "native-post-index-change"));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_refresh_tightens_owned_modes_and_never_rewrites_foreign_files() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("git-hooks");
        let old = root.join("22222222222222222222222222222222");
        fs::create_dir_all(&old).unwrap();
        let owned = old.join("post-index-change");
        write_executable(&owned, &legacy_dispatcher("post-index-change"));
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o750)).unwrap();
        let before = fs::metadata(&owned).unwrap();
        let foreign = old.join("pre-commit");
        write_executable(&foreign, "#!/bin/sh\nprintf 'user-owned hook\\n'\n");
        let foreign_bytes = fs::read(&foreign).unwrap();
        let foreign_inode = fs::metadata(&foreign).unwrap().ino();
        let unkeyed = root.join("user-hooks");
        fs::create_dir(&unkeyed).unwrap();
        let copied_header = unkeyed.join("post-index-change");
        fs::write(&copied_header, legacy_dispatcher("post-index-change")).unwrap();
        let unkeyed_bytes = fs::read(&copied_header).unwrap();
        let link = old.join("post-commit");
        std::os::unix::fs::symlink(&foreign, &link).unwrap();

        ensure_managed_git_hooks(&root.join(&managed_git_hook_set().key)).unwrap();
        assert_eq!(
            fs::read_to_string(&owned).unwrap(),
            managed_git_hook_contents("post-index-change")
        );
        let after = fs::metadata(&owned).unwrap();
        assert_ne!(
            before.ino(),
            after.ino(),
            "refresh must publish a new inode atomically"
        );
        assert_eq!(after.permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read(&foreign).unwrap(), foreign_bytes);
        assert_eq!(fs::metadata(&foreign).unwrap().ino(), foreign_inode);
        assert_eq!(fs::read(&copied_header).unwrap(), unkeyed_bytes);
        assert_eq!(fs::read_link(&link).unwrap(), foreign);
        assert_eq!(
            refresh_legacy_git_hooks(&root, Duration::from_secs(2))
                .unwrap()
                .rewritten,
            0
        );
    }

    #[test]
    fn legacy_refresh_bounds_enumeration() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..270 {
            fs::write(temp.path().join(format!("foreign-{index}")), "not a hook").unwrap();
        }
        let report = refresh_legacy_git_hooks(temp.path(), Duration::from_secs(30)).unwrap();
        assert_eq!(report.entries_examined, 256);
        assert!(report.bounded);
    }

    #[test]
    fn legacy_refresh_obeys_its_deadline() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("foreign"), "not a hook").unwrap();
        let report = refresh_legacy_git_hooks(temp.path(), Duration::ZERO).unwrap();
        assert_eq!(report.entries_examined, 0);
        assert!(report.bounded);
    }

    #[test]
    fn incomplete_legacy_refresh_refuses_to_launch_with_unvisited_dispatchers() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..270 {
            fs::write(temp.path().join(format!("foreign-{index}")), "not a hook").unwrap();
        }
        let error =
            ensure_managed_git_hooks(&temp.path().join(&managed_git_hook_set().key)).unwrap_err();
        assert!(error.contains("refresh was bounded"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_bounds_its_probes_and_does_not_feed_them_hook_stdin() {
        use std::process::Stdio;

        let temp = tempfile::tempdir().unwrap();
        let hook = temp.path().join("post-index-change");
        write_executable(&hook, &managed_git_hook_contents("post-index-change"));
        let git = temp.path().join("git");
        for body in [
            "#!/bin/sh\nread value\nprintf 'probe read stdin\\n' >&2\n",
            "#!/bin/sh\nexec sleep 60\n",
        ] {
            write_executable(&git, body);
            let mut command = Command::new(&hook);
            command
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        temp.path().display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            crate::bash_background::process::start_new_session(&mut command);
            let mut child = command.spawn().unwrap();
            // Keep the writer open: a probe that inherits hook stdin cannot
            // finish its read, even though it was not given any input.
            let input = child.stdin.take().unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    crate::bash_background::process::terminate_process(&mut child);
                    panic!("dispatcher probe waited beyond its deadline: {body}");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            drop(input);
            if body.contains("sleep") {
                assert!(!status.success(), "a stalled resolver must fail closed");
            } else {
                assert!(status.success());
            }
        }
    }

    #[cfg(unix)]
    struct DispatcherProcessFixture {
        temp: tempfile::TempDir,
        sessions: Vec<u32>,
        initial_pids: HashSet<u32>,
    }

    #[cfg(unix)]
    fn dispatcher_process_snapshot() -> Vec<(u32, u32, String)> {
        let output = Command::new("ps")
            .args(["-axo", "pid=,ppid=,pgid=,sess=,stat=,command="])
            .output()
            .unwrap();
        assert!(output.status.success(), "ps failed: {output:?}");
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| {
                let mut columns = line.split_whitespace();
                let pid = columns.next()?.parse().ok()?;
                columns.next()?; // Parent PID is retained in the diagnostic line.
                let pgid = columns.next()?.parse().ok()?;
                columns.next()?; // macOS reports an opaque session identifier.
                let state = columns.next()?;
                // Zombies cannot run or retain a cwd. Their eventual reaping by
                // init is outside the dispatcher's control after a hook SIGKILL.
                (!state.starts_with('Z')).then(|| (pid, pgid, line.to_owned()))
            })
            .collect()
    }

    #[cfg(unix)]
    impl DispatcherProcessFixture {
        fn new(git_body: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            assert!(Command::new("git")
                .args(["-c", "core.hooksPath=/dev/null", "init", "--quiet"])
                .arg(&repo)
                .status()
                .unwrap()
                .success());
            write_executable(&temp.path().join("git"), git_body);
            Self {
                temp,
                sessions: Vec::new(),
                initial_pids: dispatcher_process_snapshot()
                    .into_iter()
                    .map(|(pid, _, _)| pid)
                    .collect(),
            }
        }

        fn spawn(&mut self, contents: &str) -> std::process::Child {
            use std::process::Stdio;

            let hook = self.temp.path().join("post-index-change");
            write_executable(&hook, contents);
            // Every invocation has a fresh session/process group. POSIX sh does
            // not enable job control here, so all descendants (including ones
            // reparented to init) keep this group. This scopes ps to processes
            // started by the test without relying on command names or PPIDs.
            let mut command = Command::new("/bin/sh");
            command
                .arg(&hook)
                .current_dir(self.temp.path().join("repo"))
                .env("AFT_TEST_REPO", self.temp.path().join("repo"))
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        self.temp.path().display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            crate::bash_background::process::start_new_session(&mut command);
            let child = command.spawn().unwrap();
            self.sessions.push(child.id());
            child
        }

        fn remaining_processes(&self) -> Vec<String> {
            dispatcher_process_snapshot()
                .into_iter()
                .filter(|(pid, pgid, _)| {
                    !self.initial_pids.contains(pid) && self.sessions.contains(pgid)
                })
                .map(|(_, _, line)| line)
                .collect()
        }

        fn assert_drained(&self, allowance: Duration) {
            let deadline = Instant::now() + allowance;
            loop {
                let remaining = self.remaining_processes();
                if remaining.is_empty() {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "dispatcher left live descendants (pid ppid pgid sess stat command):\n{}",
                    remaining.join("\n")
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn wait(child: &mut std::process::Child, allowance: Duration) -> std::process::ExitStatus {
            let deadline = Instant::now() + allowance;
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    return status;
                }
                if Instant::now() >= deadline {
                    let remaining: Vec<_> = dispatcher_process_snapshot()
                        .into_iter()
                        .filter(|(_, pgid, _)| *pgid == child.id())
                        .map(|(pid, _, line)| {
                            #[cfg(target_os = "linux")]
                            {
                                let status = fs::read_to_string(format!("/proc/{pid}/status"))
                                    .unwrap_or_default();
                                let signals: Vec<_> = status
                                    .lines()
                                    .filter(|line| line.starts_with("Sig"))
                                    .collect();
                                format!("{line}\n{}", signals.join("\n"))
                            }
                            #[cfg(not(target_os = "linux"))]
                            {
                                let _ = pid;
                                line
                            }
                        })
                        .collect();
                    crate::bash_background::process::terminate_process(child);
                    panic!(
                        "dispatcher exceeded {allowance:?} (pid ppid pgid sess stat command):\n{}",
                        remaining.join("\n")
                    );
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    #[cfg(unix)]
    impl Drop for DispatcherProcessFixture {
        fn drop(&mut self) {
            // A failing orphan assertion must not itself leave test processes.
            for &session in &self.sessions {
                unsafe { libc::kill(-(session as libc::pid_t), libc::SIGKILL) };
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_fast_probes_leave_no_orphan_processes() {
        let mut fixture = DispatcherProcessFixture::new(
            "#!/bin/sh\ncase $1 in\nrev-parse) printf '%s\\n' \"$AFT_TEST_REPO\";;\nconfig) printf '%s\\n' \"$AFT_TEST_REPO/missing-hooks\";;\nesac\n",
        );
        let hook = managed_git_hook_contents("post-index-change");
        // A loaded runner can delay a healthy hook for more than two seconds.
        // Allow scheduling slack, but stay below the ten-second watchdog so a
        // stuck fast probe cannot pass merely by waiting for its timer to expire.
        let fast_probe_allowance = Duration::from_secs(6);
        for _ in 0..200 {
            let mut child = fixture.spawn(&hook);
            assert!(DispatcherProcessFixture::wait(&mut child, fast_probe_allowance).success());
        }
        // Model scheduler latency without generating CPU load. This delay is
        // before any probe/watchdog starts, so it cannot mask a leaked timer.
        let slow_start = hook.replacen("#!/bin/sh", "#!/bin/sh\n/bin/sleep 3", 1);
        let mut child = fixture.spawn(&slow_start);
        assert!(DispatcherProcessFixture::wait(&mut child, fast_probe_allowance).success());
        // Force the timer-publication interleaving without synthetic CPU load:
        // a synchronous, test-only delay preserves $!, but gives the parent
        // time to cancel before the watchdog's next command. The production
        // dispatcher above also runs 200 times without any injected delay.
        let delayed = hook.replace("sleep 10 &", "sleep 10 &\n    /bin/sleep 0.05");
        assert_ne!(hook, delayed, "test delay must reach the real watchdog");
        let mut child = fixture.spawn(&delayed);
        assert!(DispatcherProcessFixture::wait(&mut child, fast_probe_allowance).success());
        // Give exiting children scheduling slack, not enough time for an orphan
        // sleep 10 from the final publication-window probe to expire naturally.
        fixture.assert_drained(Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_cancels_a_timer_that_ignores_term() {
        let mut fixture = DispatcherProcessFixture::new(
            "#!/bin/sh\nwhile [ ! -f \"$AFT_TEST_REPO/timer-ready\" ]; do /bin/sleep 0.01; done\n",
        );
        // TERM sent between fork and exec can be consumed by the timer child's
        // inherited shell trap. Model an equally unresponsive timer explicitly,
        // and let the probe finish only after that timer is known to be running.
        write_executable(
            &fixture.temp.path().join("sleep"),
            "#!/bin/sh\ntrap '' TERM\n: > \"$AFT_TEST_REPO/timer-ready\"\nexec /bin/sleep \"$@\"\n",
        );
        let hook = managed_git_hook_contents("post-index-change");
        let bounded = hook.split("\nprobe_value()").next().unwrap();
        let mut child = fixture.spawn(&format!("{bounded}\nbounded_probe git\nexit \"$?\"\n"));
        assert!(DispatcherProcessFixture::wait(&mut child, Duration::from_secs(6)).success());
        fixture.assert_drained(Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_deadline_probe_returns_124_without_orphan_processes() {
        let mut fixture = DispatcherProcessFixture::new("#!/bin/sh\nexec sleep 600\n");
        // Exercise the exact generated function independently of probe_value,
        // whose caller deliberately converts a timeout into hook failure (1).
        let hook = managed_git_hook_contents("post-index-change");
        let bounded = hook.split("\nprobe_value()").next().unwrap();
        let mut child = fixture.spawn(&format!("{bounded}\nbounded_probe git\nexit \"$?\"\n"));
        assert_eq!(
            DispatcherProcessFixture::wait(&mut child, Duration::from_secs(16)).code(),
            Some(124)
        );
        fixture.assert_drained(Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_sigkilled_hook_descendants_exit_within_deadline() {
        let mut fixture = DispatcherProcessFixture::new("#!/bin/sh\nexec sleep 600\n");
        let mut child = fixture.spawn(&managed_git_hook_contents("post-index-change"));
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            if fixture
                .remaining_processes()
                .iter()
                .any(|line| line.ends_with("sleep 600"))
            {
                break;
            }
            assert!(Instant::now() < deadline, "stalled probe was not reached");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Kill only the hook PID, not its session, to model an outside caller
        // that cannot run a shell trap. The surviving watchdog must still end
        // the stalled probe and then exit within its original ten-second bound.
        child.kill().unwrap();
        assert!(!DispatcherProcessFixture::wait(&mut child, Duration::from_secs(6)).success());
        fixture.assert_drained(Duration::from_secs(12));
    }

    #[cfg(unix)]
    #[test]
    fn bare_dispatcher_uses_native_hooks_after_the_toplevel_probe_reports_128() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("bare.git");
        fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "--bare", "--quiet"], &HashMap::new());
        let marker = repo.join("native-hook-argument");
        write_executable(
            &repo.join("hooks/post-update"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" > '{}'\n",
                marker.display()
            ),
        );
        let mut environment = co_author_environment(temp.path());
        environment.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
        environment.insert("GIT_CONFIG_SYSTEM".into(), "/dev/null".into());
        let managed = PathBuf::from(&environment["GIT_CONFIG_VALUE_0"]);
        let output = Command::new(managed.join("post-update"))
            .arg("refs/heads/main")
            .current_dir(&repo)
            .envs(&environment)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read_to_string(marker).unwrap(), "refs/heads/main\n");
    }

    #[cfg(unix)]
    #[test]
    fn prepare_commit_msg_adds_attribution_before_repository_hook() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        write_executable(
            &repo.join(".git/hooks/prepare-commit-msg"),
            "#!/bin/sh\ngrep -q '^Co-authored-by: Pair Agent <pair@example.test>$' \"$1\" || exit 91\nprintf '%s\\n' 'Local-Hook: after-attribution' >> \"$1\"\n",
        );

        let environment = co_author_environment(&storage);
        run_git(
            &repo,
            &["commit", "--quiet", "-m", "ordered chain"],
            &environment,
        );

        let message = commit_message(&repo);
        let co_author = message.find("Co-authored-by:").unwrap();
        let local = message.find("Local-Hook: after-attribution").unwrap();
        assert!(co_author < local);
    }

    #[cfg(unix)]
    #[test]
    fn dot_githooks_fallback_runs_when_other_candidates_are_absent() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        fs::create_dir_all(repo.join(".githooks")).unwrap();
        write_executable(
            &repo.join(".githooks/pre-commit"),
            "#!/bin/sh\nprintf '%s\\n' invoked > dot-githooks-ran\n",
        );

        let environment = co_author_environment(&storage);
        run_git(
            &repo,
            &["commit", "--quiet", "-m", "fallback"],
            &environment,
        );

        assert_eq!(
            fs::read_to_string(repo.join("dot-githooks-ran")).unwrap(),
            "invoked\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn injected_hooks_dispatch_repository_pre_push_and_preserve_stdin() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        run_git(
            &repo,
            &["commit", "--quiet", "-m", "initial"],
            &HashMap::new(),
        );
        run_git(
            temp.path(),
            &["init", "--quiet", "--bare", remote.to_str().unwrap()],
            &HashMap::new(),
        );
        run_git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
            &HashMap::new(),
        );
        write_executable(
            &repo.join(".git/hooks/pre-push"),
            "#!/bin/sh\nprintf '%s\\n' invoked > pre-push-ran\ncat > pre-push-stdin\n",
        );

        let environment = co_author_environment(&storage);
        run_git(
            &repo,
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
            &environment,
        );

        assert_eq!(
            fs::read_to_string(repo.join("pre-push-ran")).unwrap(),
            "invoked\n"
        );
        assert!(
            fs::read_to_string(repo.join("pre-push-stdin"))
                .unwrap()
                .contains("refs/heads/main"),
            "the repository hook did not receive Git's original stdin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn injected_hooks_preserve_failing_pre_commit_exit_status() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        write_executable(
            &repo.join(".git/hooks/pre-commit"),
            "#!/bin/sh\nprintf '%s\\n' invoked > pre-commit-ran\nexit 73\n",
        );

        let status = Command::new("git")
            .args(["commit", "--quiet", "-m", "blocked"])
            .current_dir(&repo)
            .envs(co_author_environment(&storage))
            .status()
            .unwrap();

        assert!(
            !status.success(),
            "a failing repository hook must block commit"
        );
        assert_eq!(
            fs::read_to_string(repo.join("pre-commit-ran")).unwrap(),
            "invoked\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn injected_hooks_respect_repo_local_lefthook_style_path() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        run_git(
            &repo,
            &["config", "core.hooksPath", ".lefthook"],
            &HashMap::new(),
        );
        fs::create_dir_all(repo.join(".lefthook")).unwrap();
        write_executable(
            &repo.join(".lefthook/pre-commit"),
            "#!/bin/sh\nprintf '%s\\n' invoked > lefthook-pre-commit-ran\n",
        );

        let environment = co_author_environment(&storage);
        run_git(
            &repo,
            &["commit", "--quiet", "-m", "custom hooks path"],
            &environment,
        );

        assert_eq!(
            fs::read_to_string(repo.join("lefthook-pre-commit-ran")).unwrap(),
            "invoked\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_hook_skips_derivation_and_chains_custom_hooks_path() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        initialize_repo(&repo);
        let environment = HashMap::new();
        run_git(
            &repo,
            &["config", "core.hooksPath", ".custom-hooks"],
            &environment,
        );
        let custom_hook = repo.join(".custom-hooks/prepare-commit-msg");
        fs::create_dir_all(custom_hook.parent().unwrap()).unwrap();
        write_executable(
            &custom_hook,
            "#!/bin/sh\nprintf '%s\\n' 'Local-Hook: custom' >> \"$1\"\n",
        );

        let mut config = Config::default();
        config.github.shim = false;
        config.git.co_author = "Pair Agent <pair@example.test>".to_string();
        let mut environment = HashMap::new();
        inject(&config, &storage, &mut environment, None).unwrap();
        assert!(!environment.contains_key(GH_SHIM_BINARY_ENV));
        run_git(
            &repo,
            &["commit", "--quiet", "-m", "explicit pair"],
            &environment,
        );

        let message = commit_message(&repo);
        assert!(message.contains("Co-authored-by: Pair Agent <pair@example.test>"));
        assert!(message.contains("Local-Hook: custom"));
    }

    #[cfg(unix)]
    #[test]
    fn worktree_scoped_hooks_path_blocks_push_in_linked_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        let storage = temp.path().join("storage");
        let worktree = temp.path().join("worktree");
        let hooks_dir = temp.path().join("worktree-hooks");

        initialize_repo(&repo);
        run_git(
            &repo,
            &["commit", "--quiet", "-m", "initial"],
            &HashMap::new(),
        );
        run_git(
            temp.path(),
            &["init", "--quiet", "--bare", remote.to_str().unwrap()],
            &HashMap::new(),
        );

        // Enable worktree-specific config isolation so that git config --worktree writes
        // to config.worktree rather than the shared repository config.
        run_git(
            &repo,
            &["config", "extensions.worktreeConfig", "true"],
            &HashMap::new(),
        );

        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "-b",
                "worktree-branch",
            ],
            &HashMap::new(),
        );

        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(
            &hooks_dir.join("pre-push"),
            "#!/bin/sh\nprintf '%s\\n' invoked > worktree-pre-push-ran\nexit 1\n",
        );

        run_git(
            &worktree,
            &[
                "config",
                "--worktree",
                "core.hooksPath",
                hooks_dir.to_str().unwrap(),
            ],
            &HashMap::new(),
        );

        let remote_url = format!("file://{}", remote.display());
        let environment = co_author_environment(&storage);

        let output = std::process::Command::new("git")
            .args([
                "push",
                "--quiet",
                &remote_url,
                "HEAD:refs/heads/worktree-branch",
            ])
            .current_dir(&worktree)
            .envs(&environment)
            .output()
            .unwrap();

        assert_eq!(
            output.status.code(),
            Some(1),
            "worktree pre-push hook with exit 1 was not honoured; output: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            fs::read_to_string(worktree.join("worktree-pre-push-ran")).unwrap(),
            "invoked\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn global_hooks_path_is_honoured_when_repo_sets_none() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        let global_config = temp.path().join("global.gitconfig");
        let global_hooks = temp.path().join("global-hooks");

        initialize_repo(&repo);
        fs::create_dir_all(&global_hooks).unwrap();
        write_executable(
            &global_hooks.join("pre-commit"),
            "#!/bin/sh\nprintf '%s\\n' global-ran > global-pre-commit-ran\n",
        );

        fs::write(
            &global_config,
            format!("[core]\n\thooksPath = {}\n", global_hooks.display()),
        )
        .unwrap();

        let mut environment = co_author_environment(&storage);
        environment.insert(
            "GIT_CONFIG_GLOBAL".to_string(),
            global_config.to_str().unwrap().to_string(),
        );

        let output = std::process::Command::new("git")
            .args(["commit", "--quiet", "-m", "global test"])
            .current_dir(&repo)
            .envs(&environment)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(repo.join("global-pre-commit-ran")).unwrap(),
            "global-ran\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hooks_path_precedence_worktree_beats_local_beats_global() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        let worktree = temp.path().join("worktree");
        let global_config = temp.path().join("global.gitconfig");
        let global_hooks = temp.path().join("global-hooks");
        let local_hooks = temp.path().join("local-hooks");
        let worktree_hooks = temp.path().join("worktree-hooks");

        initialize_repo(&repo);
        run_git(
            &repo,
            &["commit", "--quiet", "-m", "initial"],
            &HashMap::new(),
        );

        run_git(
            &repo,
            &["config", "extensions.worktreeConfig", "true"],
            &HashMap::new(),
        );
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "-b",
                "wt-branch",
            ],
            &HashMap::new(),
        );

        fs::create_dir_all(&global_hooks).unwrap();
        fs::create_dir_all(&local_hooks).unwrap();
        fs::create_dir_all(&worktree_hooks).unwrap();

        write_executable(
            &global_hooks.join("pre-commit"),
            "#!/bin/sh\nprintf '%s\\n' global > which-ran\n",
        );
        write_executable(
            &local_hooks.join("pre-commit"),
            "#!/bin/sh\nprintf '%s\\n' local > which-ran\n",
        );
        write_executable(
            &worktree_hooks.join("pre-commit"),
            "#!/bin/sh\nprintf '%s\\n' worktree > which-ran\n",
        );

        fs::write(
            &global_config,
            format!("[core]\n\thooksPath = {}\n", global_hooks.display()),
        )
        .unwrap();

        // 1. All three configured: worktree beats local beats global
        run_git(
            &repo,
            &[
                "config",
                "--local",
                "core.hooksPath",
                local_hooks.to_str().unwrap(),
            ],
            &HashMap::new(),
        );
        run_git(
            &worktree,
            &[
                "config",
                "--worktree",
                "core.hooksPath",
                worktree_hooks.to_str().unwrap(),
            ],
            &HashMap::new(),
        );

        let mut environment = co_author_environment(&storage);
        environment.insert(
            "GIT_CONFIG_GLOBAL".to_string(),
            global_config.to_str().unwrap().to_string(),
        );

        run_git(
            &worktree,
            &["commit", "--quiet", "--allow-empty", "-m", "test 1"],
            &environment,
        );
        assert_eq!(
            fs::read_to_string(worktree.join("which-ran")).unwrap(),
            "worktree\n"
        );

        // 2. Remove worktree setting: local beats global
        run_git(
            &worktree,
            &["config", "--worktree", "--unset", "core.hooksPath"],
            &HashMap::new(),
        );
        run_git(
            &worktree,
            &["commit", "--quiet", "--allow-empty", "-m", "test 2"],
            &environment,
        );
        assert_eq!(
            fs::read_to_string(worktree.join("which-ran")).unwrap(),
            "local\n"
        );

        // 3. Remove local setting: global wins
        run_git(
            &repo,
            &["config", "--local", "--unset", "core.hooksPath"],
            &HashMap::new(),
        );
        run_git(
            &worktree,
            &["commit", "--quiet", "--allow-empty", "-m", "test 3"],
            &environment,
        );
        assert_eq!(
            fs::read_to_string(worktree.join("which-ran")).unwrap(),
            "global\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_hooks_path_replaces_default_git_dir_hooks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let storage = temp.path().join("storage");
        let empty_hooks = temp.path().join("empty-hooks");

        initialize_repo(&repo);
        // Put a hook in .git/hooks that should NOT run when core.hooksPath is configured
        write_executable(
            &repo.join(".git/hooks/pre-commit"),
            "#!/bin/sh\nprintf '%s\\n' default-ran > default-ran\nexit 1\n",
        );

        fs::create_dir_all(&empty_hooks).unwrap();
        run_git(
            &repo,
            &[
                "config",
                "--local",
                "core.hooksPath",
                empty_hooks.to_str().unwrap(),
            ],
            &HashMap::new(),
        );

        let environment = co_author_environment(&storage);
        let output = std::process::Command::new("git")
            .args(["commit", "--quiet", "-m", "empty hooks dir"])
            .current_dir(&repo)
            .envs(&environment)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "commit should succeed without falling back to .git/hooks: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!repo.join("default-ran").exists());
    }

    #[cfg(unix)]
    #[test]
    fn scoped_git_config_reads_ignore_command_scope_hooks_path() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        initialize_repo(&repo);

        for scope in ["--worktree", "--local", "--global", "--system"] {
            let output = std::process::Command::new("git")
                .args(["config", scope, "--get", "core.hooksPath"])
                .current_dir(&repo)
                .envs([
                    ("GIT_CONFIG_COUNT", "1"),
                    ("GIT_CONFIG_KEY_0", "core.hooksPath"),
                    ("GIT_CONFIG_VALUE_0", "/managed/aft/hooks"),
                    ("GIT_CONFIG_GLOBAL", "/dev/null"),
                    ("GIT_CONFIG_SYSTEM", "/dev/null"),
                ])
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                !stdout.contains("/managed/aft/hooks"),
                "scope {scope} leaked command-scope core.hooksPath: {stdout}"
            );
        }
    }
}

#[cfg(test)]
mod self_referential_pin_tests {
    use super::*;

    /// A pin inside the shims dir freezes the image forever (maintain always
    /// sees link==candidate); it must refuse with deploy-path steering.
    #[test]
    fn pin_inside_shims_dir_is_refused_with_steering() {
        let dir = tempfile::tempdir().unwrap();
        let shims = dir.path().join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        let frozen = shims.join("gh-shim-image");
        std::fs::write(&frozen, b"x").unwrap();
        let error = reject_self_referential_pin(&frozen, &shims).unwrap_err();
        assert!(error.contains("self-referential"), "{error}");
        assert!(error.contains("deploy path"), "{error}");
    }

    /// Negative control: an external pin (the deploy path shape) passes this
    /// gate; if this fails, the guard over-rejects and no pin works at all.
    #[test]
    fn external_pin_is_not_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let shims = dir.path().join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        let deploy = dir.path().join("bin").join("ck-aft");
        std::fs::create_dir_all(deploy.parent().unwrap()).unwrap();
        std::fs::write(&deploy, b"x").unwrap();
        assert!(reject_self_referential_pin(&deploy, &shims).is_ok());
    }
}

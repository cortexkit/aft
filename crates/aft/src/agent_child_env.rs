//! AFT-owned environment and files for first-party agent children.
//!
//! The governance controls in this module are attached to spawned bash and PTY
//! children. AFT never edits the user's shell startup files or global Git
//! configuration, so an operator's terminal keeps its existing behavior.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;

pub const SHIMS_DIR_NAME: &str = "shims";
pub const GIT_HOOKS_DIR_NAME: &str = "git-hooks";
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
const SUBC_IDENTITY_ENV_KEYS: [&str; 2] = [
    subc_protocol::SUBC_MODULE_ID_ENV,
    subc_protocol::SUBC_LAUNCH_NONCE_ENV,
];

/// Git for Windows runs shebang hooks through its bundled POSIX shell, so the
/// same dispatcher bytes work there and on Unix. Dispatch never reads stdin and
/// ends with `exec`, preserving Git's arguments, stdin, and the repository hook's
/// exit status.
const GIT_HOOK_DISPATCHER_TEMPLATE: &str = r#"#!/bin/sh
# AFT selects this hook through the agent child's environment. It does not alter
# the repository or the user's Git configuration.
hook_name=@HOOK_NAME@
@PRE_DISPATCH@
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

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || git rev-parse --absolute-git-dir 2>/dev/null || :)
if [ -z "$repo_root" ]; then
  exit 0
fi

repo_hooks=$(git config --local core.hooksPath 2>/dev/null || :)
if [ -n "$repo_hooks" ]; then
  case "$repo_hooks" in
    /*|[A-Za-z]:[\\/]*) candidate="$repo_hooks/$hook_name" ;;
    \~/*) candidate="${HOME:-}${repo_hooks#\~}/$hook_name" ;;
    *) candidate="$repo_root/$repo_hooks/$hook_name" ;;
  esac
  dispatch_candidate "$candidate" "$@"
fi

# Do not use `git rev-parse --git-path hooks/...` here: it honors the injected
# core.hooksPath and resolves this dispatcher back to itself.
git_dir=$(git rev-parse --git-dir 2>/dev/null || :)
if [ -n "$git_dir" ]; then
  case "$git_dir" in
    /*|[A-Za-z]:[\\/]*) candidate="$git_dir/hooks/$hook_name" ;;
    *) candidate="$repo_root/$git_dir/hooks/$hook_name" ;;
  esac
  dispatch_candidate "$candidate" "$@"
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
      line=$("$AFT_GH_SHIM_BINARY" gh-shim --co-author-line 2>/dev/null || :)
    fi
    ;;
  *) line="Co-authored-by: $mode" ;;
esac

if [ -n "$line" ]; then
  identity=${line#Co-authored-by: }
  git interpret-trailers --in-place --if-exists doNothing \
    --trailer "Co-authored-by=$identity" "$msg_file" 2>/dev/null || :
fi
"#;

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
    if config.gh_shim.enabled {
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
        ensure_managed_git_hooks(&storage_root.join(GIT_HOOKS_DIR_NAME))?;
    }
    Ok(())
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
        .is_some_and(|value| Path::new(&value).ends_with(GIT_HOOKS_DIR_NAME));
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
    command.envs(environment);

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
pub fn inject(
    config: &Config,
    storage_root: &Path,
    environment: &mut HashMap<String, String>,
) -> Result<(), String> {
    // The module uses these launch-identity variables to authenticate its own
    // daemon connection. Remove them only from the child snapshot so the module
    // process retains the credentials it needs.
    environment.retain(|key, _| !is_subc_credential_env_key(key));

    let gh_enabled = config.gh_shim.enabled;
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
            .is_some_and(|value| Path::new(value).ends_with(GIT_HOOKS_DIR_NAME));
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
            storage_root
                .join(GIT_HOOKS_DIR_NAME)
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
    fs::create_dir_all(hooks_dir).map_err(|error| {
        format!(
            "failed to create child Git hooks directory {}: {error}",
            hooks_dir.display()
        )
    })?;
    let expected = MANAGED_GIT_HOOK_NAMES
        .iter()
        .map(|name| (*name, managed_git_hook_contents(name)))
        .collect::<Vec<_>>();
    quarantine_foreign_hook_entries(hooks_dir, &expected)?;
    for (name, contents) in expected {
        let hook = hooks_dir.join(name);
        write_if_changed(&hook, contents.as_bytes())?;
        #[cfg(unix)]
        set_executable(&hook)?;
    }
    Ok(())
}

fn quarantine_foreign_hook_entries(
    hooks_dir: &Path,
    expected: &[(&str, String)],
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
        fs::create_dir(&staging).map_err(|error| {
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
        fs::create_dir_all(&quarantine).map_err(|error| {
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

    fs::create_dir_all(shims_dir).map_err(|error| {
        format!(
            "failed to create gh shim directory {}: {error}",
            shims_dir.display()
        )
    })?;
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
    fs::create_dir_all(shims_dir).map_err(|error| {
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

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if fs::read(path).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    if path.is_dir() {
        return Err(format!(
            "cannot replace managed child file because it is a directory: {}",
            path.display()
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("managed child file has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "failed to create managed child directory {}: {error}",
            parent.display()
        )
    })?;
    let temporary = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    fs::write(&temporary, bytes).map_err(|error| {
        format!(
            "failed to write managed child file {}: {error}",
            temporary.display()
        )
    })?;
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
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|error| {
            format!(
                "failed to read hook permissions {}: {error}",
                path.display()
            )
        })?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, GitConfig};

    #[cfg(unix)]
    const TEST_CO_AUTHOR: &str = "Pair Agent <pair@example.test>";

    #[test]
    fn disabled_features_leave_the_requested_environment_byte_identical() {
        let mut config = Config::default();
        config.gh_shim.enabled = false;
        config.git = GitConfig::default();
        let before = HashMap::from([
            ("PATH".to_string(), "/one:/two".to_string()),
            ("CUSTOM".to_string(), "value".to_string()),
        ]);
        let mut after = before.clone();
        inject(&config, Path::new("/unused"), &mut after).unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn child_environment_strips_the_complete_subc_credential_family_before_config_gates() {
        let mut config = Config::default();
        config.gh_shim.enabled = false;
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

        inject(&config, Path::new("/unused"), &mut environment).unwrap();

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
        assert_eq!(pty_spawns, 1, "agent PTY spawn inventory drifted");
        assert_eq!(
            pty.matches("sandbox_spawn::pty_command_for_plan(").count(),
            pty_spawns,
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

        config.gh_shim.enabled = false;
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

        let mut child = Command::new("git")
            .args(args)
            .current_dir(repo)
            .envs(environment)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
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
        config.gh_shim.enabled = false;
        config.git.co_author = TEST_CO_AUTHOR.to_string();
        let mut environment = HashMap::new();
        inject(&config, storage, &mut environment).unwrap();
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
        inject(&config, storage.path(), &mut environment).unwrap();
        assert_eq!(environment.get(STORAGE_DIR_ENV), None);

        // Explicit override present: propagated verbatim so spawned children
        // stay in the same storage universe (the original leak-class fix).
        let explicit = tempfile::tempdir().unwrap();
        std::env::set_var(STORAGE_DIR_ENV, explicit.path());
        let mut environment = HashMap::new();
        inject(&config, storage.path(), &mut environment).unwrap();
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
        let hook = storage.join(GIT_HOOKS_DIR_NAME).join(PREPARE_COMMIT_MSG);
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
        let hook = storage.join(GIT_HOOKS_DIR_NAME).join(PREPARE_COMMIT_MSG);
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
        config.gh_shim.enabled = false;
        config.gh_shim.binary_path = Some(shim);
        config.git.co_author = "auto".to_string();
        let mut environment = HashMap::new();
        inject(&config, &storage, &mut environment).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn maintenance_generates_the_complete_posix_dispatcher_set() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let mut config = Config::default();
        config.gh_shim.enabled = false;
        config.git.co_author = TEST_CO_AUTHOR.to_string();

        maintain(&config, &storage).unwrap();

        let hooks_dir = storage.join(GIT_HOOKS_DIR_NAME);
        let mut generated = fs::read_dir(&hooks_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
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
        }
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_quarantines_contamination_logs_and_regenerates() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let hooks_dir = storage.join(GIT_HOOKS_DIR_NAME);
        let mut config = Config::default();
        config.gh_shim.enabled = false;
        config.git.co_author = TEST_CO_AUTHOR.to_string();
        maintain(&config, &storage).unwrap();
        fs::write(
            hooks_dir.join("pre-commit"),
            "#!/bin/sh\necho foreign lefthook fallback\n",
        )
        .unwrap();
        fs::write(hooks_dir.join("unknown-manager-hook"), "foreign\n").unwrap();

        maintain(&config, &storage).unwrap();

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
        maintain(&config, &storage).unwrap();
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
        let storage = temp.path().join("storage");
        let hooks_dir = storage.join(GIT_HOOKS_DIR_NAME);
        let mut config = Config::default();
        config.gh_shim.enabled = false;
        config.git.co_author = TEST_CO_AUTHOR.to_string();
        maintain(&config, &storage).unwrap();
        assert!(!hooks_dir.join(GIT_HOOKS_QUARANTINE_DIR_NAME).exists());

        let hook = hooks_dir.join("commit-msg");
        let mut mutated = fs::read(&hook).unwrap();
        mutated.push(b' ');
        fs::write(&hook, mutated).unwrap();
        maintain(&config, &storage).unwrap();

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
        let managed = storage.join(GIT_HOOKS_DIR_NAME);
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
        config.gh_shim.enabled = false;
        config.git.co_author = "Pair Agent <pair@example.test>".to_string();
        let mut environment = HashMap::new();
        inject(&config, &storage, &mut environment).unwrap();
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

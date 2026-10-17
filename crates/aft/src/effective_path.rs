//! Process-wide PATH enrichment for children spawned by AFT.
//!
//! Daemon launches can inherit a system-only PATH that misses the user's package
//! managers and version-manager shims. AFT initializes this module before any
//! helper threads start so later subprocesses inherit the same PATH a login
//! terminal would provide.

use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

#[cfg(unix)]
use std::collections::HashSet;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use serde::{Deserialize, Serialize};

#[cfg(not(unix))]
static EFFECTIVE_PATH: OnceLock<OsString> = OnceLock::new();

#[cfg(unix)]
#[derive(Clone, Debug)]
struct PathState {
    path: &'static OsStr,
    source: ProbeSource,
    shell: String,
    elapsed: Duration,
    cache_path: PathBuf,
    refresh_started: bool,
    log_emitted: bool,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeSource {
    Cache,
    Probe,
    Timeout,
}

#[cfg(unix)]
static EFFECTIVE_PATH_STATE: std::sync::Mutex<Option<PathState>> = std::sync::Mutex::new(None);

#[cfg(unix)]
const LOGIN_SHELL_PATH_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(unix)]
const LOGIN_SHELL_PATH_PROBE_TOTAL_BUDGET: Duration = Duration::from_secs(4);
#[cfg(unix)]
const EFFECTIVE_PATH_CACHE_SCHEMA: u32 = 1;

/// Compute and export AFT's process PATH.
///
/// Call this during process startup, before AFT starts worker threads or async
/// executors. Mutating process environment variables while other threads may be
/// reading them is not safe on Unix, so later code should read the cached value
/// with [`effective_path`] instead of calling this initializer again.
pub fn initialize_process_path() -> &'static OsStr {
    let path = effective_path();

    #[cfg(unix)]
    {
        if path != OsStr::new("") && std::env::var_os("PATH").as_deref() != Some(path) {
            std::env::set_var("PATH", path);
        }
        spawn_cached_path_refresh();
    }

    path
}

/// Emit the single startup record after logging has been initialized.
///
/// PATH discovery runs before the logger and before threads, so this is split
/// from [`initialize_process_path`] rather than delaying the environment write.
pub fn log_startup_probe_result() {
    #[cfg(unix)]
    {
        let mut guard = EFFECTIVE_PATH_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(state) = guard.as_mut() else {
            return;
        };
        if state.log_emitted {
            return;
        }
        state.log_emitted = true;
        let source = match state.source {
            ProbeSource::Cache => "cache",
            ProbeSource::Probe => "probe",
            ProbeSource::Timeout => "timeout",
        };
        log::info!(
            "login-shell PATH probe: source={source} shell={} elapsed_ms={}",
            state.shell,
            state.elapsed.as_millis()
        );
    }
}

/// Create a new `Command` with the effective PATH set on Unix.
#[cfg(unix)]
pub fn new_command<S: AsRef<OsStr>>(program: S) -> std::process::Command {
    command_with_path(program.as_ref(), effective_path())
}

#[cfg(unix)]
fn command_with_path(program: &OsStr, path: &OsStr) -> std::process::Command {
    // Startup exports the effective PATH before threads start. Reapplying even
    // the same value marks PATH as changed in Rust's Command and disables
    // posix_spawn for bare executable names, forcing a costly daemon fork.
    if std::env::var_os("PATH").as_deref() == Some(path) {
        return std::process::Command::new(program);
    }
    // The process PATH can still differ: library callers that never
    // initialized it, or a daemon started from an AFT-governed shell, whose
    // startup removes the outer gh-shim directory from the process PATH after
    // the effective PATH was computed. The child must keep the effective PATH
    // for its environment and for the executable lookup. std only does that
    // lookup itself on the fork+exec path (execvp after installing the new
    // environment), so resolve the bare name here, against the same PATH, and
    // spawn the absolute path. argv[0] keeps the bare name, as execvp would.
    let mut cmd = match resolve_in_path(program, path) {
        Some(resolved) => {
            let mut cmd = std::process::Command::new(resolved);
            cmd.arg0(program);
            cmd
        }
        None => std::process::Command::new(program),
    };
    cmd.env("PATH", path);
    cmd
}

/// Find `program` the way execvp would search `path`: the first directory
/// holding an entry this process may execute. Returns `None`, leaving the
/// lookup to the child, for names that are already paths, and for a PATH with
/// an empty or relative entry, because execvp resolves those against the
/// child's working directory, which may differ from ours.
#[cfg(unix)]
fn resolve_in_path(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    if program.is_empty() || program.as_bytes().contains(&b'/') {
        return None;
    }
    let directories = std::env::split_paths(path).collect::<Vec<_>>();
    if directories.iter().any(|directory| !directory.is_absolute()) {
        return None;
    }
    directories.into_iter().find_map(|directory| {
        let candidate = directory.join(program);
        let text = std::ffi::CString::new(candidate.as_os_str().as_bytes()).ok()?;
        let executable_file = std::fs::metadata(&candidate).is_ok_and(|meta| meta.is_file())
            // SAFETY: `text` is a valid NUL-terminated path for the call.
            && unsafe { libc::access(text.as_ptr(), libc::X_OK) } == 0;
        executable_file.then_some(candidate)
    })
}

/// On Windows the process PATH is already correct (registry-backed
/// environment block); pass through without touching the child env.
#[cfg(not(unix))]
pub fn new_command<S: AsRef<OsStr>>(program: S) -> std::process::Command {
    std::process::Command::new(program)
}

/// Return the cached PATH that subprocesses should inherit.
///
/// On Windows this is the process PATH unchanged: Windows daemon environments
/// already receive PATH from the registry-backed environment block.
#[cfg(not(unix))]
pub fn effective_path() -> &'static OsStr {
    EFFECTIVE_PATH
        .get_or_init(compute_effective_path)
        .as_os_str()
}

/// Where the probed login-shell PATH is cached between processes.
#[cfg(unix)]
pub(crate) fn effective_path_cache_path() -> PathBuf {
    crate::bash_background::storage_dir(None).join("effective-path.json")
}

#[cfg(unix)]
pub fn effective_path() -> &'static OsStr {
    // Test seam: integration tests construct exact PATHs (e.g. to simulate a
    // missing formatter binary); probing and enrichment would re-add real tool
    // dirs from the host and break that isolation. Checked at runtime because
    // the spawned test binary is a production build.
    // "0" reads as unset so the PATH feature's own integration tests can
    // opt back in to probing under a test harness that defaults the seam on.
    if std::env::var_os("AFT_TEST_RAW_PATH").is_some_and(|value| value != "0" && !value.is_empty())
    {
        static RAW: OnceLock<OsString> = OnceLock::new();
        return RAW
            .get_or_init(|| std::env::var_os("PATH").unwrap_or_default())
            .as_os_str();
    }

    let mut guard = EFFECTIVE_PATH_STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(state) = guard.as_ref() {
        return state.path;
    }

    let started = Instant::now();
    let current = std::env::var_os("PATH").unwrap_or_default();
    let home = crate::environment::non_empty_os_var("HOME");
    // Use the normal process-state resolver. It is available before the app is
    // constructed, so no early XDG-only fallback can split this cache from AFT's
    // configured storage root.
    let cache_path = effective_path_cache_path();
    let candidates = login_shell_candidates();

    let (login_path, source, shell) = read_effective_path_cache(&cache_path)
        .filter(|cache| cache_matches_current_shell(cache, &candidates))
        .map(|cache| {
            (
                cache.path.map(OsString::from),
                ProbeSource::Cache,
                cache.shell,
            )
        })
        .unwrap_or_else(|| {
            let result = probe_login_shell_path();
            let cache = EffectivePathCache {
                schema: EFFECTIVE_PATH_CACHE_SCHEMA,
                shell: result.shell.to_string_lossy().into_owned(),
                path: result
                    .path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                probed_at_unix: unix_timestamp_secs(),
                inputs: shell_startup_inputs(&result.shell),
            };
            let _ = write_effective_path_cache(&cache_path, &cache);
            let source = if result.path.is_some() {
                ProbeSource::Probe
            } else {
                ProbeSource::Timeout
            };
            (result.path, source, cache.shell)
        });

    let merged = login_path
        .as_deref()
        .map(|path| merge_current_and_login_path(&current, path))
        .unwrap_or_else(|| current.clone());
    let enriched = append_missing_standard_dirs(&merged, home.as_deref(), |dir| dir.is_dir());
    let path = Box::leak(enriched.into_boxed_os_str());
    *guard = Some(PathState {
        path,
        source,
        shell,
        elapsed: started.elapsed(),
        cache_path,
        refresh_started: false,
        log_emitted: false,
    });
    path
}

#[cfg(windows)]
fn compute_effective_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

#[cfg(not(any(unix, windows)))]
fn compute_effective_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

/// Core tool directories are kept first in the standard-directory fallback.
#[cfg(unix)]
fn core_standard_path_dirs(home: Option<&OsStr>) -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = home {
        let home = PathBuf::from(home);
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs
}

/// All dirs merged into every constructed PATH when present on disk. Includes
/// common installer locations that may not be represented in a shell probe.
#[cfg(unix)]
fn user_standard_path_dirs(home: Option<&OsStr>) -> Vec<PathBuf> {
    let mut dirs = core_standard_path_dirs(home);
    if let Some(home) = home {
        let home = PathBuf::from(home);
        dirs.push(home.join(".bun/bin"));
        dirs.push(home.join("Library/pnpm"));
        dirs.push(home.join(".local/share/pnpm"));
        dirs.push(home.join(".local/share/mise/shims"));
        dirs.push(home.join(".deno/bin"));
        dirs.push(home.join(".volta/bin"));
    }
    dirs
}

#[cfg(unix)]
fn append_missing_standard_dirs<D>(
    path: &OsStr,
    home: Option<&OsStr>,
    mut dir_exists: D,
) -> OsString
where
    D: FnMut(&Path) -> bool,
{
    let mut entries: Vec<PathBuf> = std::env::split_paths(path).collect();
    let mut seen: HashSet<PathBuf> = entries.iter().cloned().collect();

    for dir in user_standard_path_dirs(home) {
        if dir_exists(&dir) && seen.insert(dir.clone()) {
            entries.push(dir);
        }
    }

    std::env::join_paths(entries).unwrap_or_else(|_| path.to_os_string())
}

#[cfg(unix)]
#[derive(Debug)]
struct LoginPathProbe {
    shell: PathBuf,
    path: Option<OsString>,
}

#[cfg(unix)]
fn probe_login_shell_path() -> LoginPathProbe {
    let candidates = login_shell_candidates();
    let fallback_shell = candidates
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("/bin/sh"));
    let deadline = Instant::now() + LOGIN_SHELL_PATH_PROBE_TOTAL_BUDGET;

    for shell in candidates {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let timeout = LOGIN_SHELL_PATH_PROBE_TIMEOUT.min(deadline.saturating_duration_since(now));
        if let Some(path) = probe_login_shell_path_once(&shell, timeout) {
            if login_path_is_acceptable(&path) {
                // Cache against the requested shell, not a fallback that happened
                // to answer this probe. Otherwise a hanging $SHELL would pay its
                // timeout again on every launch before the fallback can be reused.
                return LoginPathProbe {
                    shell: fallback_shell,
                    path: Some(path),
                };
            }
        }
    }

    LoginPathProbe {
        shell: fallback_shell,
        path: None,
    }
}

#[cfg(unix)]
fn login_shell_candidates() -> Vec<PathBuf> {
    // This runtime seam lets integration tests exercise the production binary
    // with a deterministic shell. Normal launches still use SHELL and fall back
    // to the common interactive shells below.
    if let Some(value) = crate::environment::non_empty_os_var("AFT_TEST_LOGIN_SHELL_CANDIDATES") {
        let candidates = std::env::split_paths(&value).collect::<Vec<_>>();
        if !candidates.is_empty() {
            return candidates;
        }
    }

    let mut candidates = Vec::new();
    if let Some(shell) = std::env::var_os("SHELL").filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(shell));
    }
    let zsh = PathBuf::from("/bin/zsh");
    let bash = PathBuf::from("/bin/bash");
    if !candidates.contains(&zsh) {
        candidates.push(zsh);
    }
    if !candidates.contains(&bash) {
        candidates.push(bash);
    }
    candidates
}

#[cfg(unix)]
fn set_nonblocking<F: AsRawFd>(file: &F) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn probe_login_shell_path_once(shell: &Path, timeout: Duration) -> Option<OsString> {
    let mut command = Command::new(shell);

    command
        .arg(probe_shell_flags(shell))
        .arg(probe_shell_command(shell))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Run the probe in its own session so the timeout can kill login-shell
    // startup helpers as well as the shell process itself.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let _ = set_nonblocking(&stdout);
    let mut output_bytes = Vec::new();
    let mut buf = [0u8; 1024];

    let deadline = Instant::now() + timeout;
    loop {
        use std::io::Read;
        match stdout.read(&mut buf) {
            Ok(0) => {
                // EOF can arrive before a shell-startup child exits. Keep the
                // existing one-second reap grace, but never let it overrun the
                // caller's per-candidate share of the total startup budget.
                let wait_deadline = (Instant::now() + Duration::from_secs(1)).min(deadline);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() >= wait_deadline => {
                            kill_login_shell_probe(&mut child);
                            break;
                        }
                        Ok(None) => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => {
                            kill_login_shell_probe(&mut child);
                            break;
                        }
                    }
                }
                return extract_probe_path(&output_bytes);
            }
            Ok(n) => {
                output_bytes.extend_from_slice(&buf[..n]);
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                kill_login_shell_probe(&mut child);
                return None;
            }
        }

        match child.try_wait() {
            Ok(Some(_)) => {
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => output_bytes.extend_from_slice(&buf[..n]),
                        Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let _ = child.wait();
                return extract_probe_path(&output_bytes);
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_login_shell_probe(&mut child);
                return None;
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                kill_login_shell_probe(&mut child);
                return None;
            }
        }
    }
}

#[cfg(unix)]
fn probe_shell_flags(_shell: &Path) -> &'static str {
    // Login and interactive modes together cover the startup files that may
    // contribute PATH entries, including bash's .bashrc and zsh's .zshrc.
    "-lic"
}

#[cfg(unix)]
fn probe_shell_command(shell: &Path) -> &'static str {
    let name = shell.file_name().and_then(|name| name.to_str());
    if name.is_some_and(|name| name.eq_ignore_ascii_case("fish")) {
        r#"printf '\n__AFT_PATH_BEGIN__%s__AFT_PATH_END__\n' (string join : $PATH)"#
    } else {
        r#"printf '\n__AFT_PATH_BEGIN__%s__AFT_PATH_END__\n' "$PATH""#
    }
}

#[cfg(unix)]
fn extract_probe_path(output: &[u8]) -> Option<OsString> {
    const BEGIN: &[u8] = b"__AFT_PATH_BEGIN__";
    const END: &[u8] = b"__AFT_PATH_END__";
    let begin = output
        .windows(BEGIN.len())
        .position(|window| window == BEGIN)?
        + BEGIN.len();
    let end = output[begin..]
        .windows(END.len())
        .position(|window| window == END)?
        + begin;
    Some(OsString::from_vec(output[begin..end].to_vec()))
}

#[cfg(unix)]
fn kill_login_shell_probe(child: &mut std::process::Child) {
    let pid = child.id() as i32;
    if pid > 0 {
        // Negative PID targets the process group created by setsid above.
        unsafe {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn login_path_is_acceptable(path: &OsStr) -> bool {
    let bytes = path.as_bytes();
    if bytes.is_empty()
        || bytes
            .iter()
            .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r'))
    {
        return false;
    }

    if bytes.contains(&b' ') {
        let mut abs_count = 0;
        for part in bytes.split(|&byte| byte == b' ') {
            if part.first() == Some(&b'/') {
                abs_count += 1;
            }
        }
        if abs_count > 1 {
            return false;
        }
    }

    bytes
        .split(|byte| *byte == b':')
        .all(|entry| entry.first() == Some(&b'/'))
}

#[cfg(unix)]
fn merge_current_and_login_path(current_path: &OsStr, login_path: &OsStr) -> OsString {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();

    // Keep daemon-provided ordering and precedence; shell startup files only
    // contribute entries that are not already present, appended at the end.
    for entry in std::env::split_paths(current_path).chain(std::env::split_paths(login_path)) {
        if seen.insert(entry.clone()) {
            merged.push(entry);
        }
    }

    std::env::join_paths(merged).unwrap_or_else(|_| current_path.to_os_string())
}

#[cfg(unix)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct EffectivePathInput {
    file: String,
    mtime_ns: Option<i128>,
    size: Option<u64>,
}

#[cfg(unix)]
#[derive(Clone, Debug, Deserialize, Serialize)]
struct EffectivePathCache {
    schema: u32,
    shell: String,
    path: Option<String>,
    probed_at_unix: u64,
    inputs: Vec<EffectivePathInput>,
}

#[cfg(unix)]
fn read_effective_path_cache(path: &Path) -> Option<EffectivePathCache> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

#[cfg(unix)]
fn write_effective_path_cache(path: &Path, cache: &EffectivePathCache) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        crate::private_storage::create_dir_all(parent)?;
    }
    let content = serde_json::to_vec(cache).map_err(std::io::Error::other)?;
    let temporary = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("effective-path.json"),
        std::process::id()
    ));
    let result = (|| {
        crate::private_storage::write(&temporary, content)?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn cache_matches_current_shell(cache: &EffectivePathCache, candidates: &[PathBuf]) -> bool {
    let Some(current_shell) = candidates.first() else {
        return false;
    };
    cache.schema == EFFECTIVE_PATH_CACHE_SCHEMA
        && cache.shell == current_shell.to_string_lossy()
        && cache.inputs == shell_startup_inputs(current_shell)
        && cache
            .path
            .as_deref()
            .map(|path| login_path_is_acceptable(OsStr::new(path)))
            .unwrap_or(true)
}

#[cfg(unix)]
fn shell_startup_inputs(shell: &Path) -> Vec<EffectivePathInput> {
    let name = shell.file_name().and_then(|name| name.to_str());
    let is_zsh = name.is_some_and(|name| name.eq_ignore_ascii_case("zsh"));
    let mut files = if is_zsh {
        vec![
            PathBuf::from("/etc/zshenv"),
            PathBuf::from("/etc/zprofile"),
            PathBuf::from("/etc/zshrc"),
        ]
    } else {
        vec![PathBuf::from("/etc/profile")]
    };
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        let home = PathBuf::from(home);
        let names: &[&str] = if is_zsh {
            &[".zshenv", ".zprofile", ".zshrc"]
        } else {
            &[".bash_profile", ".bash_login", ".profile", ".bashrc"]
        };
        files.extend(names.iter().map(|name| home.join(name)));
    }
    files
        .into_iter()
        .map(|file| match std::fs::metadata(&file) {
            Ok(metadata) => EffectivePathInput {
                file: file.to_string_lossy().into_owned(),
                mtime_ns: Some(
                    i128::from(metadata.mtime()) * 1_000_000_000
                        + i128::from(metadata.mtime_nsec()),
                ),
                size: Some(metadata.len()),
            },
            Err(_) => EffectivePathInput {
                file: file.to_string_lossy().into_owned(),
                mtime_ns: None,
                size: None,
            },
        })
        .collect()
}

#[cfg(unix)]
fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Refresh a cache from the helper process without mutating its environment.
#[cfg(unix)]
pub fn refresh_login_shell_path_cache(cache_path: &Path) -> Result<(), String> {
    let result = probe_login_shell_path();
    let cache = EffectivePathCache {
        schema: EFFECTIVE_PATH_CACHE_SCHEMA,
        shell: result.shell.to_string_lossy().into_owned(),
        path: result
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        probed_at_unix: unix_timestamp_secs(),
        inputs: shell_startup_inputs(&result.shell),
    };
    write_effective_path_cache(cache_path, &cache)
        .map_err(|error| format!("write {}: {error}", cache_path.display()))
}

#[cfg(not(unix))]
pub fn refresh_login_shell_path_cache(_cache_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn spawn_cached_path_refresh() {
    // Integration tests point this seam at a deliberately sleeping shell and
    // assert the serving binary does not run it on a cache hit. Production
    // launches do not set the seam and always refresh through the helper.
    if std::env::var_os("AFT_TEST_LOGIN_SHELL_CANDIDATES").is_some() {
        return;
    }

    let cache_path = {
        let mut guard = EFFECTIVE_PATH_STATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(state) = guard.as_mut() else {
            return;
        };
        if state.source != ProbeSource::Cache || state.refresh_started {
            return;
        }
        state.refresh_started = true;
        state.cache_path.clone()
    };

    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .arg("--probe-login-shell-path")
        .arg(cache_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Run the cache-refresh helper independently so it can finish after the
    // serving process exits. Its shell probe still enforces the per-candidate
    // timeout and kills the probe process group when necessary.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    spawn_reaped(&mut command);
}

/// Spawn `command` and hand the child to a short-lived thread that waits on
/// it, returning the child pid so callers can observe the reap.
///
/// `setsid()` only detaches the helper from the session; the spawning process
/// remains its parent and is the only one that can reap it. Without a
/// `wait()` the finished helper stays a zombie for the life of this process.
#[cfg(unix)]
fn spawn_reaped(command: &mut Command) -> Option<u32> {
    let Ok(child) = command.spawn() else {
        return None;
    };
    let pid = child.id();
    reap_child_in_background(child);
    Some(pid)
}

/// Reap `child` from a named background thread so the caller never blocks.
#[cfg(unix)]
fn reap_child_in_background(mut child: Child) {
    // When the thread cannot be spawned, the error drops the moved closure
    // and its child with it: the child goes unreaped, matching the behavior
    // callers had before the reaper existed, instead of blocking on wait().
    let _ = std::thread::Builder::new()
        .name("aft-path-refresh-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// Test seam that tells which spawn mechanism std used for a `Command`.
///
/// A `pthread_atfork` prepare handler counts fork() calls made by the current
/// thread. std's fork+exec fallback calls libc `fork`, which runs the handler;
/// its `posix_spawn` path (a syscall on macOS, a CLONE_VFORK clone on glibc)
/// does not. The count is per thread, so parallel tests that fork do not
/// disturb each other.
#[cfg(all(test, unix))]
pub(crate) mod fork_counter {
    use std::cell::Cell;
    use std::sync::Once;

    thread_local! {
        static FORKS: Cell<usize> = const { Cell::new(0) };
    }

    extern "C" fn count_fork() {
        FORKS.with(|forks| forks.set(forks.get() + 1));
    }

    /// Run `body` and return its result with the number of forks this thread
    /// made meanwhile.
    pub(crate) fn forks_during<T>(body: impl FnOnce() -> T) -> (T, usize) {
        static REGISTER: Once = Once::new();
        REGISTER.call_once(|| {
            // SAFETY: the handler only touches a const-initialized thread-local.
            let rc = unsafe { libc::pthread_atfork(Some(count_fork), None, None) };
            assert_eq!(rc, 0, "pthread_atfork failed");
        });
        let before = FORKS.with(Cell::get);
        let value = body();
        (value, FORKS.with(Cell::get) - before)
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    #[test]
    fn helpers_do_not_override_inherited_path() {
        let path = std::env::var_os("PATH").unwrap();
        let command = super::command_with_path(std::ffi::OsStr::new("git"), &path);
        assert!(
            !command.get_envs().any(|(key, _)| key == "PATH"),
            "an explicit PATH, even unchanged, forces Rust's macOS Command to fork"
        );
    }

    #[test]
    fn helpers_preserve_distinct_effective_path() {
        let path = std::ffi::OsStr::new("/aft-test-distinct-path");
        let command = super::command_with_path(std::ffi::OsStr::new("git"), path);
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "PATH" && value == Some(path)));
    }

    /// A PATH that differs from the process PATH must not push bare-name
    /// helpers (git above all) onto std's fork+exec fallback, and the child
    /// must still find the tool on, and inherit, that PATH.
    #[test]
    fn distinct_effective_path_spawns_bare_names_without_fork() {
        let tools = tempfile::tempdir().unwrap();
        let tool = tools.path().join("aft-path-probe-tool");
        fs::write(&tool, "#!/bin/sh\nprintf '%s' \"$PATH\"\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let path =
            std::env::join_paths([tools.path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
        assert_ne!(std::env::var_os("PATH").as_deref(), Some(path.as_os_str()));

        let workdir = tempfile::tempdir().unwrap();
        let (output, forks) = fork_counter::forks_during(|| {
            let mut command = command_with_path(OsStr::new("aft-path-probe-tool"), &path);
            command.current_dir(workdir.path()).output().unwrap()
        });
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            output.stdout,
            path.as_bytes(),
            "child keeps the effective PATH"
        );
        assert_eq!(
            forks, 0,
            "bare-name spawn under a distinct PATH used fork+exec"
        );

        // The fork counter must see the fork of a pre_exec spawn; otherwise the
        // zero forks asserted above would not show that posix_spawn was used.
        let (_, control) = fork_counter::forks_during(|| {
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", "exit 0"]);
            // SAFETY: the closure does nothing.
            unsafe { command.pre_exec(|| Ok(())) };
            command.status().unwrap()
        });
        assert_eq!(control, 1, "fork counter missed a pre_exec spawn");
    }

    #[test]
    fn path_resolution_defers_relative_entries_and_paths_to_the_child() {
        let tools = tempfile::tempdir().unwrap();
        let tool = tools.path().join("aft-path-probe-tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let absolute = std::env::join_paths([tools.path()]).unwrap();
        assert_eq!(
            resolve_in_path(OsStr::new("aft-path-probe-tool"), &absolute),
            Some(tool.clone())
        );
        let with_relative = std::env::join_paths([Path::new("bin"), tools.path()]).unwrap();
        assert_eq!(
            resolve_in_path(OsStr::new("aft-path-probe-tool"), &with_relative),
            None
        );
        assert_eq!(resolve_in_path(tool.as_os_str(), &absolute), None);
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            resolve_in_path(OsStr::new("aft-path-probe-tool"), &absolute),
            None,
            "a non-executable entry is skipped, as execvp would"
        );
    }

    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct EnvVarGuard {
        key: &'static str,
        old_value: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old_value = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old_value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(val) = &self.old_value {
                std::env::set_var(self.key, val);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn write_executable_shim(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn probe_entries_are_appended_after_current_entries_without_duplicates() {
        let current = OsStr::new("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
        let login = OsString::from("/usr/bin:/custom/bin:/opt/homebrew/bin");

        let effective = merge_current_and_login_path(current, &login);

        assert_eq!(
            effective,
            OsString::from("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/custom/bin")
        );
    }

    #[test]
    fn probe_shell_flags_cover_login_and_interactive_startup_files() {
        assert_eq!(probe_shell_flags(Path::new("/bin/zsh")), "-lic");
        assert_eq!(probe_shell_flags(Path::new("/bin/bash")), "-lic");
        assert_eq!(
            probe_shell_flags(Path::new("/opt/homebrew/bin/fish")),
            "-lic"
        );
    }

    #[test]
    fn marker_extraction_ignores_shell_startup_noise() {
        let output = b"banner before\n__AFT_PATH_BEGIN__/custom/bin:/usr/bin__AFT_PATH_END__\nbanner after\n";

        assert_eq!(
            extract_probe_path(output),
            Some(OsString::from("/custom/bin:/usr/bin"))
        );
    }

    /// Generous budget for tests that assert a PROBE SUCCEEDS (spawning a
    /// real shim): decoupled from the production timeout so machine load
    /// cannot convert a working probe into a test failure.
    const TEST_PROBE_SUCCESS_TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    fn zsh_probe_uses_interactive_login_and_reads_zshrc() {
        let _guard = crate::test_env::process_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let custom_bin = home.join(".custom/bin");
        let shell = dir.path().join("zsh");
        fs::create_dir_all(&custom_bin).unwrap();
        fs::write(
            home.join(".zshrc"),
            format!(
                "printf 'banner before\n'; export PATH=\"$PATH:{}\"; printf 'banner after\n'\n",
                custom_bin.display()
            ),
        )
        .unwrap();
        write_executable_shim(
            &shell,
            r#"#!/bin/sh
if [ "$1" != '-lic' ]; then
  exit 64
fi
if [ -f "$ZDOTDIR/.zshrc" ]; then
  . "$ZDOTDIR/.zshrc"
fi
eval "$2"
"#,
        );

        let _path_guard = EnvVarGuard::set("PATH", "/usr/bin:/bin");
        let _home_guard = EnvVarGuard::set("HOME", home.to_str().unwrap());
        let _zdotdir_guard = EnvVarGuard::set("ZDOTDIR", home.to_str().unwrap());
        // Warm the freshly written shim with one untimed exec: macOS assesses
        // never-seen executables on first exec (syspolicyd), which can take
        // tens of seconds on a loaded machine and would read as a probe
        // failure. The timed probe below then measures the shell, not the OS.
        let _ = Command::new(&shell).arg("--warmup").output();
        // Success-asserting probe tests use a generous budget: the production
        // 3s timeout is a startup-cost bound, and a loaded machine can push a
        // real shim spawn past it, which reads as a false test failure.
        let probed = probe_login_shell_path_once(&shell, TEST_PROBE_SUCCESS_TIMEOUT);

        assert_eq!(
            probed,
            Some(OsString::from(format!(
                "/usr/bin:/bin:{}",
                custom_bin.display()
            )))
        );
    }

    #[test]
    fn invalid_probe_paths_are_rejected() {
        let rejected = vec![
            OsString::new(),
            OsString::from("/fake/login/bin\n/usr/bin"),
            OsString::from("/fake/login/bin:relative/bin"),
            OsString::from_vec(b"/fake/login/bin\0/usr/bin".to_vec()),
            OsString::from("/usr/bin /bin /opt/homebrew/bin"), // fish-shaped space-joined
        ];

        for probe_path in rejected {
            assert!(!login_path_is_acceptable(&probe_path));
        }
    }

    #[test]
    fn inline_probe_total_budget_caps_multiple_hanging_candidates() {
        let _guard = crate::test_env::process_env_lock();
        let dir = tempfile::tempdir().expect("create tempdir");
        let first = dir.path().join("first-shell");
        let second = dir.path().join("second-shell");
        write_executable_shim(&first, "#!/bin/sh\nsleep 10\n");
        write_executable_shim(&second, "#!/bin/sh\nsleep 10\n");
        let candidates = std::env::join_paths([&first, &second]).unwrap();
        let _candidates_guard = EnvVarGuard::set(
            "AFT_TEST_LOGIN_SHELL_CANDIDATES",
            candidates.to_str().unwrap(),
        );

        let started = Instant::now();
        let result = probe_login_shell_path();

        assert!(result.path.is_none());
        assert!(
            started.elapsed() < Duration::from_millis(4500),
            "two hanging candidates exceeded the four-second total budget"
        );
    }

    #[test]
    fn login_shell_probe_times_out() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let shell = dir.path().join("slow-login-shell");
        fs::write(
            &shell,
            "#!/bin/sh\nsleep 10\nprintf '%s' '/fake/login/bin:/usr/bin:/bin'\n",
        )
        .expect("write fake shell");
        let mut permissions = fs::metadata(&shell)
            .expect("fake shell metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&shell, permissions).expect("chmod fake shell");

        let started = Instant::now();
        let probed = probe_login_shell_path_once(&shell, LOGIN_SHELL_PATH_PROBE_TIMEOUT);

        assert!(probed.is_none());
        // Upper bound proves the 3s timeout fired instead of waiting out the
        // 10s sleep; 8s leaves headroom for a loaded machine to reap the kill.
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "login-shell PATH probe exceeded the 8s test budget"
        );
    }

    #[test]
    fn test_login_shell_candidates_includes_fallbacks() {
        let _guard = crate::test_env::process_env_lock();
        let _shell_guard = EnvVarGuard::set("SHELL", "/opt/zerobrew/bin/fish");
        let candidates = login_shell_candidates();
        assert_eq!(candidates[0], PathBuf::from("/opt/zerobrew/bin/fish"));
        assert!(candidates.contains(&PathBuf::from("/bin/zsh")));
        assert!(candidates.contains(&PathBuf::from("/bin/bash")));
    }

    #[test]
    fn effective_path_cache_round_trips_and_rejects_changed_inputs() {
        let _guard = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let _home_guard = EnvVarGuard::set("HOME", home.to_str().unwrap());
        let shell = PathBuf::from("/bin/bash");
        let cache_path = temp.path().join("effective-path.json");
        let cache = EffectivePathCache {
            schema: EFFECTIVE_PATH_CACHE_SCHEMA,
            shell: shell.to_string_lossy().into_owned(),
            path: Some("/custom/bin:/usr/bin:/bin".to_string()),
            probed_at_unix: unix_timestamp_secs(),
            inputs: shell_startup_inputs(&shell),
        };

        write_effective_path_cache(&cache_path, &cache).unwrap();
        let restored = read_effective_path_cache(&cache_path).unwrap();
        assert!(cache_matches_current_shell(&restored, &[shell.clone()]));

        fs::write(home.join(".bashrc"), "export PATH=changed\n").unwrap();
        assert!(!cache_matches_current_shell(&restored, &[shell]));
    }

    #[test]
    fn probe_failure_falls_back_to_current_path_and_appends_standard_dirs() {
        let home = Path::new("/home/alice");
        let current = OsStr::new("/usr/bin:/bin");
        let missing_shell = Path::new("/nonexistent/shell");

        assert!(probe_login_shell_path_once(missing_shell, Duration::from_millis(10)).is_none());

        let enriched = append_missing_standard_dirs(current, Some(home.as_os_str()), |dir| {
            dir == Path::new("/home/alice/.bun/bin")
        });
        assert_eq!(
            enriched,
            OsString::from("/usr/bin:/bin:/home/alice/.bun/bin")
        );
    }

    #[test]
    fn test_append_missing_standard_dirs() {
        let home = OsStr::new("/home/alice");
        let path = OsStr::new("/usr/bin:/bin");

        // Mock dir_exists to return true only for ~/.bun/bin
        let dir_exists = |dir: &Path| dir == Path::new("/home/alice/.bun/bin");

        let enriched = append_missing_standard_dirs(path, Some(home), dir_exists);
        assert_eq!(
            enriched,
            OsString::from("/usr/bin:/bin:/home/alice/.bun/bin")
        );
    }

    #[test]
    fn test_append_missing_standard_dirs_dedup_and_order() {
        let home = OsStr::new("/home/alice");
        // ~/.bun/bin is already in the path, but /opt/homebrew/bin is missing
        let path = OsStr::new("/home/alice/.bun/bin:/usr/bin:/bin");

        let dir_exists = |dir: &Path| {
            dir == Path::new("/home/alice/.bun/bin") || dir == Path::new("/opt/homebrew/bin")
        };

        let enriched = append_missing_standard_dirs(path, Some(home), dir_exists);
        // /opt/homebrew/bin should be appended at the end, and ~/.bun/bin should not be duplicated
        assert_eq!(
            enriched,
            OsString::from("/home/alice/.bun/bin:/usr/bin:/bin:/opt/homebrew/bin")
        );
    }

    #[test]
    fn reaper_thread_reaps_spawned_child() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let pid = spawn_reaped(&mut command).expect("helper spawn failed");

        // A finished child that nobody waited on stays in the process table
        // as a zombie and still answers kill(pid, 0); only a reaped child
        // disappears. Poll with a bounded deadline until the pid is gone,
        // which proves the reaper thread called wait().
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
            if status == -1 {
                let error = std::io::Error::last_os_error();
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::ESRCH),
                    "kill(pid, 0) failed with an unexpected error: {error}"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "child {pid} was still in the process table after the reap deadline"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

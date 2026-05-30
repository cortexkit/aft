//! External tool runner and auto-formatter detection.
//!
//! Provides subprocess execution with timeout protection, language-to-formatter
//! mapping, and the `auto_format` entry point used by `write_format_validate`.

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::parser::{detect_language, LangId};

/// Result of running an external tool subprocess.
#[derive(Debug)]
pub struct ExternalToolResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub truncated: bool,
}

struct SubprocessOutcome {
    stdout: String,
    stderr: String,
    status: ExitStatus,
    truncated: bool,
}

/// Errors from external tool execution.
#[derive(Debug)]
pub enum FormatError {
    /// The tool binary was not found on PATH.
    NotFound { tool: String },
    /// The tool exceeded its timeout and was killed.
    Timeout { tool: String, timeout_secs: u32 },
    /// The tool exited with a non-zero status.
    Failed { tool: String, stderr: String },
    /// No formatter is configured for this language.
    UnsupportedLanguage,
}

/// A configured formatter/checker that cannot be resolved for configure warnings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MissingTool {
    pub kind: String,
    pub language: String,
    pub tool: String,
    pub hint: String,
}

#[derive(Debug, Clone)]
struct ToolCandidate {
    tool: String,
    source: String,
    args: Vec<String>,
    required: bool,
}

#[derive(Debug, Clone)]
enum ToolDetection {
    Found(String, Vec<String>),
    NotConfigured,
    NotInstalled { tool: String },
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::NotFound { tool } => write!(f, "formatter not found: {}", tool),
            FormatError::Timeout { tool, timeout_secs } => {
                write!(f, "formatter '{}' timed out after {}s", tool, timeout_secs)
            }
            FormatError::Failed { tool, stderr } => {
                write!(f, "formatter '{}' failed: {}", tool, stderr)
            }
            FormatError::UnsupportedLanguage => write!(f, "unsupported language for formatting"),
        }
    }
}

/// Apply Unix-specific isolation so a kill() on timeout terminates
/// grandchildren too (e.g. `sh -c 'sleep 60'` orphaning `sleep`).
///
/// Without this, killing the immediate child (`sh`) leaves `sleep`
/// holding stdout/stderr pipes open, and the reader threads block
/// until `sleep` terminates — turning a 2s timeout into a 60s hang.
#[cfg(unix)]
fn isolate_in_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn isolate_in_process_group(_cmd: &mut Command) {
    // Best-effort no-op outside Unix. Windows timeout cleanup uses taskkill /T
    // in kill_process_tree so .cmd wrappers and grandchildren are terminated.
}

/// Kill the child and (on Unix) its entire process group, so orphaned
/// grandchildren don't keep pipes open after a timeout.
#[cfg(unix)]
fn kill_process_tree(child: &mut Child) {
    let pid = child.id() as i32;
    if pid > 0 {
        // SAFETY: killpg with SIGKILL on a process group leader is safe.
        // Negative pid form (kill -pgid) targets the whole group.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(windows)]
fn kill_process_tree(child: &mut Child) {
    let pid = child.id().to_string();
    let _ = Command::new("taskkill")
        .args(["/PID", pid.as_str(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

#[cfg(not(any(unix, windows)))]
fn kill_process_tree(child: &mut Child) {
    let _ = child.kill();
}

/// Spawn a subprocess and wait for completion with timeout protection.
///
/// Polls `try_wait()` at 50ms intervals. On timeout, kills the child process
/// and waits for it to exit. Returns `FormatError::NotFound` when the binary
/// isn't on PATH.
pub fn run_external_tool(
    command: &str,
    args: &[&str],
    working_dir: Option<&Path>,
    timeout_secs: u32,
) -> Result<ExternalToolResult, FormatError> {
    let mut cmd = Command::new(command);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    isolate_in_process_group(&mut cmd);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(FormatError::NotFound {
                tool: command.to_string(),
            });
        }
        Err(e) => {
            return Err(FormatError::Failed {
                tool: command.to_string(),
                stderr: e.to_string(),
            });
        }
    };

    let outcome = wait_with_timeout(child, command, timeout_secs)?;
    let exit_code = outcome.status.code().unwrap_or(-1);
    if exit_code != 0 {
        return Err(FormatError::Failed {
            tool: command.to_string(),
            stderr: outcome.stderr,
        });
    }

    Ok(ExternalToolResult {
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        exit_code,
        truncated: outcome.truncated,
    })
}

const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;

fn wait_with_timeout(
    mut child: Child,
    command: &str,
    timeout_secs: u32,
) -> Result<SubprocessOutcome, FormatError> {
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_thread =
        thread::spawn(move || read_bounded_to_string(stdout_pipe, MAX_CAPTURE_BYTES));
    let stderr_thread =
        thread::spawn(move || read_bounded_to_string(stderr_pipe, MAX_CAPTURE_BYTES));
    let deadline = Instant::now() + Duration::from_secs(timeout_secs as u64);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (stdout, stdout_truncated) = stdout_thread.join().unwrap_or_default();
                let (stderr, stderr_truncated) = stderr_thread.join().unwrap_or_default();
                return Ok(SubprocessOutcome {
                    stdout,
                    stderr,
                    status,
                    truncated: stdout_truncated || stderr_truncated,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_process_tree(&mut child);
                    let _ = child.wait();
                    // Do NOT block joining the reader threads — orphaned
                    // grandchildren may still hold the pipes open even after
                    // the immediate child is gone. The threads will detach
                    // and clean up when pipes finally close.
                    return Err(FormatError::Timeout {
                        tool: command.to_string(),
                        timeout_secs,
                    });
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                kill_process_tree(&mut child);
                let _ = child.wait();
                // Same rationale as the timeout branch: don't block on join.
                return Err(FormatError::Failed {
                    tool: command.to_string(),
                    stderr: format!("try_wait error: {}", e),
                });
            }
        }
    }
}

fn read_bounded_to_string<R: Read>(mut reader: R, limit: usize) -> (String, bool) {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut scratch = [0u8; 8192];
    let mut truncated = false;

    loop {
        let read = match reader.read(&mut scratch) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };

        let remaining = limit.saturating_sub(bytes.len());
        if remaining > 0 {
            let keep = remaining.min(read);
            bytes.extend_from_slice(&scratch[..keep]);
            if keep < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }

    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}

/// TTL for tool availability and resolution cache entries.
const TOOL_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolCacheKey {
    command: String,
    project_root: PathBuf,
}

static TOOL_RESOLUTION_CACHE: std::sync::LazyLock<
    Mutex<HashMap<ToolCacheKey, (Option<PathBuf>, Instant)>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

static TOOL_AVAILABILITY_CACHE: std::sync::LazyLock<Mutex<HashMap<String, (bool, Instant)>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn tool_cache_key(command: &str, project_root: Option<&Path>) -> ToolCacheKey {
    ToolCacheKey {
        command: command.to_string(),
        project_root: project_root.map(Path::to_path_buf).unwrap_or_default(),
    }
}

fn availability_cache_key(command: &str, project_root: Option<&Path>) -> String {
    let root = project_root
        .map(|path| path.to_string_lossy())
        .unwrap_or_default();
    format!("{}\0{}", command, root)
}

pub fn clear_tool_cache() {
    if let Ok(mut cache) = TOOL_RESOLUTION_CACHE.lock() {
        cache.clear();
    }
    if let Ok(mut cache) = TOOL_AVAILABILITY_CACHE.lock() {
        cache.clear();
    }
}

/// Resolve a tool by checking node_modules/.bin relative to project_root, then PATH.
/// Returns the full path to the tool if found, otherwise None.
fn resolve_tool(command: &str, project_root: Option<&Path>) -> Option<String> {
    let key = tool_cache_key(command, project_root);
    if let Ok(cache) = TOOL_RESOLUTION_CACHE.lock() {
        if let Some((resolved, checked_at)) = cache.get(&key) {
            if checked_at.elapsed() < TOOL_CACHE_TTL {
                return resolved
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string());
            }
        }
    }

    let resolved = resolve_tool_uncached(command, project_root);
    if let Ok(mut cache) = TOOL_RESOLUTION_CACHE.lock() {
        cache.insert(key, (resolved.clone(), Instant::now()));
    }
    resolved.map(|path| path.to_string_lossy().to_string())
}

pub(crate) fn resolve_tool_uncached(command: &str, project_root: Option<&Path>) -> Option<PathBuf> {
    // 1. Check node_modules/.bin/<command> relative to project root. On
    // Windows, package managers usually create .cmd/.bat/.ps1 shims rather
    // than extensionless executables, so probe PATHEXT-style variants too.
    if let Some(root) = project_root {
        let local_bin_dir = root.join("node_modules").join(".bin");
        for local_bin in local_node_bin_candidates(&local_bin_dir, command) {
            if local_bin.exists() {
                return Some(local_bin);
            }
        }
    }

    // 2. Try PATH lookup first. This is the fast common path: spawning the
    // tool with command-specific availability arguments and waiting briefly
    // for it to exit. When the editor (OpenCode, Pi, etc.) is launched from a
    // login shell the PATH is usually complete, so this finds Homebrew/cargo/etc.
    // binaries.
    if let Some(path) = try_path_lookup(command) {
        return Some(path);
    }

    // 3. Fall back to well-known install locations the editor's PATH may
    // not contain. GitHub issue #47: macOS GUI launches (Spotlight, Dock,
    // Alfred) and some Linux desktop launchers drop /opt/homebrew/bin and
    // similar from PATH, making PATH lookups fail even though the user
    // genuinely has the tool installed. Returning the absolute path here
    // means downstream `Command::new(resolved)` works regardless.
    try_well_known_path_lookup(command)
}

fn local_node_bin_candidates(bin_dir: &Path, command: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let command_path = Path::new(command);
        if command_path.extension().is_some() {
            return vec![bin_dir.join(command)];
        }

        let mut candidates = vec![bin_dir.join(command)];
        candidates.extend(
            windows_local_node_bin_extensions(std::env::var_os("PATHEXT").as_deref())
                .into_iter()
                .map(|ext| bin_dir.join(format!("{command}{ext}"))),
        );
        candidates
    }

    #[cfg(not(windows))]
    {
        vec![bin_dir.join(command)]
    }
}

#[cfg(any(windows, test))]
fn windows_local_node_bin_extensions(pathext: Option<&std::ffi::OsStr>) -> Vec<String> {
    const DEFAULT_ORDER: [&str; 4] = [".cmd", ".exe", ".bat", ".ps1"];
    let allowed: HashSet<&str> = DEFAULT_ORDER.into_iter().collect();

    let mut ordered = Vec::new();
    if let Some(pathext) = pathext.and_then(|value| value.to_str()) {
        for ext in pathext.split(';') {
            let normalized = ext.trim().to_ascii_lowercase();
            if allowed.contains(normalized.as_str()) && !ordered.contains(&normalized) {
                ordered.push(normalized);
            }
        }
    }

    for ext in DEFAULT_ORDER {
        if !ordered.iter().any(|existing| existing == ext) {
            ordered.push(ext.to_string());
        }
    }

    ordered
}

/// Try spawning the tool via the inherited PATH. Returns the bare command
/// name on success (downstream `Command::new` re-resolves through PATH),
/// or None if the spawn fails or the tool exits with non-zero status.
fn try_path_lookup(command: &str) -> Option<PathBuf> {
    let probe_args = path_lookup_probe_args(command);
    let mut child = Command::new(command)
        .args(probe_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    let timeout = Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Some(PathBuf::from(command))
                } else {
                    None
                };
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

fn path_lookup_probe_args(command: &str) -> &'static [&'static str] {
    match command {
        // Go uses `go version` rather than a POSIX-style `--version` flag.
        "go" => &["version"],
        // `gofmt` has no version flag. It exits successfully with empty stdin,
        // which is sufficient for PATH availability probing.
        "gofmt" => &[],
        _ => &["--version"],
    }
}

/// Look up `command` in the well-known install locations that GUI-launched
/// editors commonly miss from PATH. Returns the absolute path so the caller
/// invokes the tool via `Command::new(absolute_path)` regardless of PATH.
///
/// Search order is built by `well_known_search_paths`:
/// 1. `/opt/homebrew/bin` (Apple Silicon Homebrew)
/// 2. `/usr/local/bin` (Intel Mac Homebrew + most manual Linux installs)
/// 3. `/usr/local/go/bin` (official go.dev installer)
/// 4. `/usr/bin` (distro-packaged tools)
/// 5. `/snap/bin` (snap-packaged tools)
/// 6. `$HOME/.cargo/bin` (cargo install — rustfmt, etc.)
/// 7. `$HOME/go/bin` (`go install` default GOPATH layout)
/// 8. `$HOME/.local/bin` (pip --user, pipx, npm prefix, many shell scripts)
///
/// Each candidate is verified to (a) exist as a regular file and (b) be
/// executable; we don't spawn `--version` here because spawning an
/// absolute-path candidate that doesn't accept `--version` would emit a
/// false negative (and Rust's `fs::metadata` is much cheaper than a spawn).
fn try_well_known_path_lookup(command: &str) -> Option<PathBuf> {
    // Test-only escape hatch: integration tests that need to assert
    // "tool not installed" semantics set AFT_DISABLE_WELL_KNOWN_LOOKUP=1
    // so CI runners with a system tsc/biome/etc. at /usr/local/bin don't
    // silently make those tests pass. Production callers never set this.
    if std::env::var_os("AFT_DISABLE_WELL_KNOWN_LOOKUP").is_some() {
        return None;
    }
    if cfg!(windows) {
        // On Windows, check common install locations that GUI-launched editors
        // may miss from PATH: Go SDK, Cargo, and user-local Go binaries.
        let candidates =
            well_known_windows_search_paths(command, std::env::var_os("USERPROFILE").as_deref());
        return try_well_known_path_lookup_in(&candidates);
    }
    let candidates = well_known_search_paths(command, std::env::var_os("HOME").as_deref());
    try_well_known_path_lookup_in(&candidates)
}

/// Build the candidate path list for the given command name and HOME value.
/// Extracted so tests can drive the lookup with a controlled HOME without
/// mutating process-global env vars.
fn well_known_search_paths(command: &str, home: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(8);
    candidates.push(PathBuf::from("/opt/homebrew/bin").join(command));
    candidates.push(PathBuf::from("/usr/local/bin").join(command));
    // System/distro install locations a GUI-launched editor's truncated PATH
    // often misses. /usr/local/go/bin is where the official go.dev installer
    // puts the Go toolchain (gofmt, go); /snap/bin and /usr/bin cover
    // distro-packaged installs (Go from apt/snap, etc.).
    candidates.push(PathBuf::from("/usr/local/go/bin").join(command));
    candidates.push(PathBuf::from("/usr/bin").join(command));
    candidates.push(PathBuf::from("/snap/bin").join(command));
    if let Some(home) = home {
        let home_path = PathBuf::from(home);
        candidates.push(home_path.join(".cargo/bin").join(command));
        candidates.push(home_path.join("go/bin").join(command));
        candidates.push(home_path.join(".local/bin").join(command));
    }
    candidates
}

/// Build the candidate path list for the given command name using well-known
/// Windows install locations. Extracted so tests can drive the lookup with a
/// controlled USERPROFILE without mutating process-global env vars.
///
/// Search order:
/// 1. `C:\Go\bin\<command>.exe` — Windows Go installer (default path)
/// 2. `C:\Program Files\Go\bin\<command>.exe` — Windows Go installer (Program Files)
/// 3. `%USERPROFILE%\.cargo\bin\<command>.exe` — `cargo install`
/// 4. `%USERPROFILE%\go\bin\<command>.exe` — `go install` with default GOPATH
///
/// Each candidate appends `.exe` because Windows executables require the
/// extension for `std::fs::metadata` to resolve the correct file.
#[cfg(windows)]
fn well_known_windows_search_paths(
    command: &str,
    userprofile: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    let exe_name = format!("{}.exe", command);
    let mut candidates: Vec<PathBuf> = Vec::with_capacity(5);
    // Go SDK installations
    candidates.push(PathBuf::from(r"C:\Go\bin").join(&exe_name));
    candidates.push(PathBuf::from(r"C:\Program Files\Go\bin").join(&exe_name));
    if let Some(up) = userprofile {
        let up_path = PathBuf::from(up);
        // Cargo-installed tools (rustfmt, cargo-outdated, etc.)
        candidates.push(up_path.join(r".cargo\bin").join(&exe_name));
        // Go-installed tools (gopls, staticcheck, goimports, etc.)
        candidates.push(up_path.join(r"go\bin").join(&exe_name));
    }
    candidates
}

#[cfg(not(windows))]
fn well_known_windows_search_paths(
    _command: &str,
    _userprofile: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    Vec::new() // dead code on POSIX, included for compile-time completeness
}

/// Walk a pre-built candidate list, returning the first file that exists and
/// is executable. Extracted from `try_well_known_path_lookup` so tests can
/// inject candidates anchored at a tempdir.
fn try_well_known_path_lookup_in(candidates: &[PathBuf]) -> Option<PathBuf> {
    for candidate in candidates {
        if let Ok(metadata) = std::fs::metadata(candidate) {
            if metadata.is_file() && is_executable(&metadata) {
                return Some(candidate.clone());
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    // Windows: the well-known Windows paths in `try_well_known_path_lookup`
    // construct .exe paths which are always executable (or the metadata check
    // already filters out non-files). This stub exists for compile-time
    // completeness on the POSIX candidate path used during non-Windows builds.
    true
}

/// Check if `ruff format` is available with a stable formatter.
///
/// Ruff's formatter became stable in v0.1.2. Versions before that output
/// `NOT_YET_IMPLEMENTED_*` stubs instead of formatted code. We parse the
/// version from `ruff --version` (format: "ruff X.Y.Z") and require >= 0.1.2.
/// Falls back to false if ruff is not found or version cannot be parsed.
fn ruff_format_available(project_root: Option<&Path>) -> bool {
    let key = availability_cache_key("ruff-format", project_root);
    if let Ok(cache) = TOOL_AVAILABILITY_CACHE.lock() {
        if let Some((available, checked_at)) = cache.get(&key) {
            if checked_at.elapsed() < TOOL_CACHE_TTL {
                return *available;
            }
        }
    }

    let result = ruff_format_available_uncached(project_root);
    if let Ok(mut cache) = TOOL_AVAILABILITY_CACHE.lock() {
        cache.insert(key, (result, Instant::now()));
    }
    result
}

fn ruff_format_available_uncached(project_root: Option<&Path>) -> bool {
    let command = match resolve_tool("ruff", project_root) {
        Some(command) => command,
        None => return false,
    };
    let output = match Command::new(&command)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };

    let version_str = String::from_utf8_lossy(&output.stdout);
    // Parse "ruff X.Y.Z" or just "X.Y.Z"
    let version_part = version_str
        .trim()
        .strip_prefix("ruff ")
        .unwrap_or(version_str.trim());

    let parts: Vec<&str> = version_part.split('.').collect();
    if parts.len() < 3 {
        return false;
    }

    let major: u32 = match parts[0].parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let minor: u32 = match parts[1].parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let patch: u32 = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => return false,
    };

    // Require >= 0.1.2 where ruff format became stable
    (major, minor, patch) >= (0, 1, 2)
}

fn resolve_candidate_tool(
    candidate: &ToolCandidate,
    project_root: Option<&Path>,
    require_ruff_format: bool,
) -> Option<String> {
    if require_ruff_format && candidate.tool == "ruff" && !ruff_format_available(project_root) {
        return None;
    }

    resolve_tool(&candidate.tool, project_root)
}

fn lang_key(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => "typescript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Solidity => "solidity",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Yaml => "yaml",
    }
}

fn has_formatter_support(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::TypeScript
            | LangId::JavaScript
            | LangId::Tsx
            | LangId::Python
            | LangId::Rust
            | LangId::Go
    )
}

fn has_checker_support(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::TypeScript
            | LangId::JavaScript
            | LangId::Tsx
            | LangId::Python
            | LangId::Rust
            | LangId::Go
    )
}

fn formatter_candidates(lang: LangId, config: &Config, file_str: &str) -> Vec<ToolCandidate> {
    let project_root = config.project_root.as_deref();
    if let Some(preferred) = config.formatter.get(lang_key(lang)) {
        return explicit_formatter_candidate(preferred, file_str);
    }

    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => {
            if has_project_config(project_root, &["biome.json", "biome.jsonc"]) {
                vec![ToolCandidate {
                    tool: "biome".to_string(),
                    source: "biome.json".to_string(),
                    args: vec![
                        "format".to_string(),
                        "--write".to_string(),
                        file_str.to_string(),
                    ],
                    required: true,
                }]
            } else if has_project_config(
                project_root,
                &[".oxfmtrc.json", ".oxfmtrc.jsonc", "oxfmt.config.ts"],
            ) {
                vec![ToolCandidate {
                    tool: "oxfmt".to_string(),
                    source: "oxfmt config".to_string(),
                    args: vec!["--write".to_string(), file_str.to_string()],
                    required: true,
                }]
            } else if has_project_config(
                project_root,
                &[
                    ".prettierrc",
                    ".prettierrc.json",
                    ".prettierrc.yml",
                    ".prettierrc.yaml",
                    ".prettierrc.js",
                    ".prettierrc.cjs",
                    ".prettierrc.mjs",
                    ".prettierrc.toml",
                    "prettier.config.js",
                    "prettier.config.cjs",
                    "prettier.config.mjs",
                ],
            ) {
                vec![ToolCandidate {
                    tool: "prettier".to_string(),
                    source: "Prettier config".to_string(),
                    args: vec!["--write".to_string(), file_str.to_string()],
                    required: true,
                }]
            } else if has_project_config(project_root, &["deno.json", "deno.jsonc"]) {
                vec![ToolCandidate {
                    tool: "deno".to_string(),
                    source: "deno.json".to_string(),
                    args: vec!["fmt".to_string(), file_str.to_string()],
                    required: true,
                }]
            } else {
                Vec::new()
            }
        }
        LangId::Python => {
            if has_project_config(project_root, &["ruff.toml", ".ruff.toml"])
                || has_pyproject_tool(project_root, "ruff")
            {
                vec![ToolCandidate {
                    tool: "ruff".to_string(),
                    source: "ruff config".to_string(),
                    args: vec!["format".to_string(), file_str.to_string()],
                    required: true,
                }]
            } else if has_pyproject_tool(project_root, "black") {
                vec![ToolCandidate {
                    tool: "black".to_string(),
                    source: "pyproject.toml".to_string(),
                    args: vec![file_str.to_string()],
                    required: true,
                }]
            } else {
                Vec::new()
            }
        }
        LangId::Rust => {
            if has_project_config(project_root, &["Cargo.toml"]) {
                vec![ToolCandidate {
                    tool: "rustfmt".to_string(),
                    source: "Cargo.toml".to_string(),
                    args: vec![file_str.to_string()],
                    required: true,
                }]
            } else {
                Vec::new()
            }
        }
        LangId::Go => {
            if has_project_config(project_root, &["go.mod"]) {
                vec![
                    ToolCandidate {
                        tool: "goimports".to_string(),
                        source: "go.mod".to_string(),
                        args: vec!["-w".to_string(), file_str.to_string()],
                        required: false,
                    },
                    ToolCandidate {
                        tool: "gofmt".to_string(),
                        source: "go.mod".to_string(),
                        args: vec!["-w".to_string(), file_str.to_string()],
                        required: true,
                    },
                ]
            } else {
                Vec::new()
            }
        }
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl => Vec::new(),
        LangId::Html => Vec::new(),
        LangId::Markdown => Vec::new(),
        LangId::Yaml => Vec::new(),
    }
}

fn checker_candidates(lang: LangId, config: &Config, file_str: &str) -> Vec<ToolCandidate> {
    let project_root = config.project_root.as_deref();
    if let Some(preferred) = config.checker.get(lang_key(lang)) {
        return explicit_checker_candidate(preferred, file_str);
    }

    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => {
            if has_project_config(project_root, &["biome.json", "biome.jsonc"]) {
                vec![ToolCandidate {
                    tool: "biome".to_string(),
                    source: "biome.json".to_string(),
                    args: vec![
                        "check".to_string(),
                        "--reporter=json".to_string(),
                        file_str.to_string(),
                    ],
                    required: true,
                }]
            } else if has_project_config(project_root, &["tsconfig.json"]) {
                vec![ToolCandidate {
                    tool: "tsc".to_string(),
                    source: "tsconfig.json".to_string(),
                    args: vec![
                        "--noEmit".to_string(),
                        "--pretty".to_string(),
                        "false".to_string(),
                    ],
                    required: true,
                }]
            } else {
                Vec::new()
            }
        }
        LangId::Python => {
            if has_project_config(project_root, &["pyrightconfig.json"])
                || has_pyproject_tool(project_root, "pyright")
            {
                vec![ToolCandidate {
                    tool: "pyright".to_string(),
                    source: "pyright config".to_string(),
                    args: vec!["--outputjson".to_string(), file_str.to_string()],
                    required: true,
                }]
            } else if has_project_config(project_root, &["ruff.toml", ".ruff.toml"])
                || has_pyproject_tool(project_root, "ruff")
            {
                vec![ToolCandidate {
                    tool: "ruff".to_string(),
                    source: "ruff config".to_string(),
                    args: vec![
                        "check".to_string(),
                        "--output-format=json".to_string(),
                        file_str.to_string(),
                    ],
                    required: true,
                }]
            } else {
                Vec::new()
            }
        }
        LangId::Rust => {
            if has_project_config(project_root, &["Cargo.toml"]) {
                vec![ToolCandidate {
                    tool: "cargo".to_string(),
                    source: "Cargo.toml".to_string(),
                    args: vec!["check".to_string(), "--message-format=json".to_string()],
                    required: true,
                }]
            } else {
                Vec::new()
            }
        }
        LangId::Go => {
            if has_project_config(project_root, &["go.mod"]) {
                vec![
                    ToolCandidate {
                        tool: "staticcheck".to_string(),
                        source: "go.mod".to_string(),
                        args: vec!["-f".to_string(), "json".to_string(), file_str.to_string()],
                        required: false,
                    },
                    ToolCandidate {
                        tool: "go".to_string(),
                        source: "go.mod".to_string(),
                        args: vec!["vet".to_string(), file_str.to_string()],
                        required: true,
                    },
                ]
            } else {
                Vec::new()
            }
        }
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl => Vec::new(),
        LangId::Html => Vec::new(),
        LangId::Markdown => Vec::new(),
        LangId::Yaml => Vec::new(),
    }
}

fn explicit_formatter_candidate(name: &str, file_str: &str) -> Vec<ToolCandidate> {
    match name {
        "none" | "off" | "false" => Vec::new(),
        "biome" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "formatter config".to_string(),
            args: vec![
                "format".to_string(),
                "--write".to_string(),
                file_str.to_string(),
            ],
            required: true,
        }],
        "oxfmt" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "formatter config".to_string(),
            args: vec!["--write".to_string(), file_str.to_string()],
            required: true,
        }],
        "prettier" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "formatter config".to_string(),
            args: vec!["--write".to_string(), file_str.to_string()],
            required: true,
        }],
        "deno" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "formatter config".to_string(),
            args: vec!["fmt".to_string(), file_str.to_string()],
            required: true,
        }],
        "ruff" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "formatter config".to_string(),
            args: vec!["format".to_string(), file_str.to_string()],
            required: true,
        }],
        "black" | "rustfmt" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "formatter config".to_string(),
            args: vec![file_str.to_string()],
            required: true,
        }],
        "goimports" | "gofmt" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "formatter config".to_string(),
            args: vec!["-w".to_string(), file_str.to_string()],
            required: true,
        }],
        _ => Vec::new(),
    }
}

fn explicit_checker_candidate(name: &str, file_str: &str) -> Vec<ToolCandidate> {
    match name {
        "none" | "off" | "false" => Vec::new(),
        "tsc" | "tsgo" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "checker config".to_string(),
            args: vec![
                "--noEmit".to_string(),
                "--pretty".to_string(),
                "false".to_string(),
            ],
            required: true,
        }],
        "cargo" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "checker config".to_string(),
            args: vec!["check".to_string(), "--message-format=json".to_string()],
            required: true,
        }],
        "go" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "checker config".to_string(),
            args: vec!["vet".to_string(), file_str.to_string()],
            required: true,
        }],
        "biome" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "checker config".to_string(),
            args: vec![
                "check".to_string(),
                "--reporter=json".to_string(),
                file_str.to_string(),
            ],
            required: true,
        }],
        "pyright" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "checker config".to_string(),
            args: vec!["--outputjson".to_string(), file_str.to_string()],
            required: true,
        }],
        "ruff" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "checker config".to_string(),
            args: vec![
                "check".to_string(),
                "--output-format=json".to_string(),
                file_str.to_string(),
            ],
            required: true,
        }],
        "staticcheck" => vec![ToolCandidate {
            tool: name.to_string(),
            source: "checker config".to_string(),
            args: vec!["-f".to_string(), "json".to_string(), file_str.to_string()],
            required: true,
        }],
        _ => Vec::new(),
    }
}

fn resolve_tool_candidates(
    candidates: Vec<ToolCandidate>,
    project_root: Option<&Path>,
    require_ruff_format: bool,
) -> ToolDetection {
    if candidates.is_empty() {
        return ToolDetection::NotConfigured;
    }

    let mut missing_required = None;
    for candidate in candidates {
        if let Some(command) = resolve_candidate_tool(&candidate, project_root, require_ruff_format)
        {
            return ToolDetection::Found(command, candidate.args);
        }
        if candidate.required && missing_required.is_none() {
            missing_required = Some(candidate.tool);
        }
    }

    match missing_required {
        Some(tool) => ToolDetection::NotInstalled { tool },
        None => ToolDetection::NotConfigured,
    }
}

fn checker_command(_candidate: &ToolCandidate, resolved: String) -> String {
    resolved
}

fn checker_args(candidate: &ToolCandidate) -> Vec<String> {
    if candidate.tool == "tsc" || candidate.tool == "tsgo" {
        vec![
            "--noEmit".to_string(),
            "--pretty".to_string(),
            "false".to_string(),
        ]
    } else {
        candidate.args.clone()
    }
}

fn detect_formatter_for_path(path: &Path, lang: LangId, config: &Config) -> ToolDetection {
    let file_str = path.to_string_lossy().to_string();
    resolve_tool_candidates(
        formatter_candidates(lang, config, &file_str),
        config.project_root.as_deref(),
        true,
    )
}

fn detect_checker_for_path(path: &Path, lang: LangId, config: &Config) -> ToolDetection {
    let file_str = path.to_string_lossy().to_string();
    let candidates = checker_candidates(lang, config, &file_str);
    if candidates.is_empty() {
        return ToolDetection::NotConfigured;
    }

    let project_root = config.project_root.as_deref();
    let mut missing_required = None;
    for candidate in candidates {
        if let Some(command) = resolve_candidate_tool(&candidate, project_root, false) {
            return ToolDetection::Found(
                checker_command(&candidate, command),
                checker_args(&candidate),
            );
        }
        if candidate.required && missing_required.is_none() {
            missing_required = Some(candidate.tool);
        }
    }

    match missing_required {
        Some(tool) => ToolDetection::NotInstalled { tool },
        None => ToolDetection::NotConfigured,
    }
}

fn languages_in_project(project_root: &Path) -> HashSet<LangId> {
    crate::callgraph::walk_project_files(project_root)
        .filter_map(|path| detect_language(&path))
        .collect()
}

fn placeholder_file_for_language(project_root: &Path, lang: LangId) -> PathBuf {
    let filename = match lang {
        LangId::TypeScript => "aft-tool-detection.ts",
        LangId::Tsx => "aft-tool-detection.tsx",
        LangId::JavaScript => "aft-tool-detection.js",
        LangId::Python => "aft-tool-detection.py",
        LangId::Rust => "aft_tool_detection.rs",
        LangId::Go => "aft_tool_detection.go",
        LangId::C => "aft_tool_detection.c",
        LangId::Cpp => "aft_tool_detection.cpp",
        LangId::Zig => "aft_tool_detection.zig",
        LangId::CSharp => "aft_tool_detection.cs",
        LangId::Bash => "aft_tool_detection.sh",
        LangId::Solidity => "aft_tool_detection.sol",
        LangId::Vue => "aft-tool-detection.vue",
        LangId::Json => "aft-tool-detection.json",
        LangId::Scala => "aft-tool-detection.scala",
        LangId::Java => "aft-tool-detection.java",
        LangId::Ruby => "aft-tool-detection.rb",
        LangId::Kotlin => "aft-tool-detection.kt",
        LangId::Swift => "aft-tool-detection.swift",
        LangId::Php => "aft-tool-detection.php",
        LangId::Lua => "aft-tool-detection.lua",
        LangId::Perl => "aft-tool-detection.pl",
        LangId::Html => "aft-tool-detection.html",
        LangId::Markdown => "aft-tool-detection.md",
        LangId::Yaml => "aft-tool-detection.yaml",
    };
    project_root.join(filename)
}

pub(crate) fn install_hint(tool: &str) -> String {
    match tool {
        "biome" => {
            "Run `bun add -d --workspace-root @biomejs/biome` or install globally.".to_string()
        }
        "oxfmt" => "Run `npm install -D oxfmt` or install globally.".to_string(),
        "prettier" => "Run `npm install -D prettier` or install globally.".to_string(),
        "tsc" => "Run `npm install -D typescript` or install globally.".to_string(),
        "tsgo" => {
            "Run `npm install -D @typescript/native-preview` or install globally.".to_string()
        }
        "pyright" | "pyright-langserver" => "Install: `npm install -g pyright`".to_string(),
        "ruff" => {
            "Install: `pip install ruff` or your Python package manager equivalent.".to_string()
        }
        "black" => {
            "Install: `pip install black` or your Python package manager equivalent.".to_string()
        }
        "rustfmt" => "Install: `rustup component add rustfmt`".to_string(),
        "rust-analyzer" => "Install: `rustup component add rust-analyzer`".to_string(),
        "cargo" => "Install Rust from https://rustup.rs/.".to_string(),
        "go" => if cfg!(windows) {
            "Install Go from https://go.dev/dl/. Common install paths: \
                 C:\\Go\\bin, C:\\Program Files\\Go\\bin. \
                 GUI-launched editors often don't inherit login-shell PATH."
        } else {
            "Install Go from https://go.dev/dl/, or — if it's already installed — \
                 ensure its bin directory is on PATH (Homebrew typically uses \
                 /opt/homebrew/bin on Apple Silicon, /usr/local/bin on Intel macOS). \
                 GUI-launched editors often don't inherit login-shell PATH."
        }
        .to_string(),
        "gopls" => "Install: `go install golang.org/x/tools/gopls@latest`".to_string(),
        "bash-language-server" => "Install: `npm install -g bash-language-server`".to_string(),
        "yaml-language-server" => "Install: `npm install -g yaml-language-server`".to_string(),
        "typescript-language-server" => {
            "Install: `npm install -g typescript-language-server typescript`".to_string()
        }
        "deno" => "Install Deno from https://deno.com/.".to_string(),
        "goimports" => "Install: `go install golang.org/x/tools/cmd/goimports@latest`".to_string(),
        "staticcheck" => {
            "Install: `go install honnef.co/go/tools/cmd/staticcheck@latest`".to_string()
        }
        other => format!("Install `{other}` and ensure it is on PATH."),
    }
}

fn configured_tool_hint(tool: &str, source: &str) -> String {
    // GitHub issue #47: editors launched from a non-login GUI shell (Spotlight,
    // Dock, Alfred, etc.) often don't inherit the user's full PATH, so a tool
    // that's installed but lives under /opt/homebrew/bin, ~/.cargo/bin, or
    // similar can fail this lookup. We already check those well-known
    // locations in `resolve_tool_uncached`; if we still didn't find the tool,
    // it's genuinely missing OR sits in an unusual install prefix.
    //
    // Word the message so users know to check both "is it installed at all"
    // and "is it on AFT's PATH" — rather than implying definite absence.
    format!(
        "{tool} is configured in {source} but was not found on PATH or in common install locations. {}",
        install_hint(tool)
    )
}

fn missing_tool_warning(
    kind: &str,
    language: &str,
    candidate: &ToolCandidate,
    project_root: Option<&Path>,
    require_ruff_format: bool,
) -> Option<MissingTool> {
    if !candidate.required
        || resolve_candidate_tool(candidate, project_root, require_ruff_format).is_some()
    {
        return None;
    }

    Some(MissingTool {
        kind: kind.to_string(),
        language: language.to_string(),
        tool: candidate.tool.clone(),
        hint: configured_tool_hint(&candidate.tool, &candidate.source),
    })
}

/// Detect configured formatters/checkers that are missing for languages present in the project.
pub fn detect_missing_tools(project_root: &Path, config: &Config) -> Vec<MissingTool> {
    let languages = languages_in_project(project_root);
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();

    for lang in languages {
        let language = lang_key(lang);
        let placeholder = placeholder_file_for_language(project_root, lang);
        let file_str = placeholder.to_string_lossy().to_string();

        for candidate in formatter_candidates(lang, config, &file_str) {
            if let Some(warning) = missing_tool_warning(
                "formatter_not_installed",
                language,
                &candidate,
                config.project_root.as_deref(),
                true,
            ) {
                if seen.insert((
                    warning.kind.clone(),
                    warning.language.clone(),
                    warning.tool.clone(),
                )) {
                    warnings.push(warning);
                }
            }
        }

        for candidate in checker_candidates(lang, config, &file_str) {
            if let Some(warning) = missing_tool_warning(
                "checker_not_installed",
                language,
                &candidate,
                config.project_root.as_deref(),
                false,
            ) {
                if seen.insert((
                    warning.kind.clone(),
                    warning.language.clone(),
                    warning.tool.clone(),
                )) {
                    warnings.push(warning);
                }
            }
        }
    }

    warnings.sort_by(|left, right| {
        (&left.kind, &left.language, &left.tool).cmp(&(&right.kind, &right.language, &right.tool))
    });
    warnings
}

/// Detect the appropriate formatter command and arguments for a file.
///
/// Priority per language:
/// - TypeScript/JavaScript/TSX: `prettier --write <file>`
/// - Python: `ruff format <file>` (fallback: `black <file>`)
/// - Rust: `rustfmt <file>`
/// - Go: `gofmt -w <file>`
///
/// Returns `None` if no formatter is available for the language.
pub fn detect_formatter(
    path: &Path,
    lang: LangId,
    config: &Config,
) -> Option<(String, Vec<String>)> {
    match detect_formatter_for_path(path, lang, config) {
        ToolDetection::Found(cmd, args) => Some((cmd, args)),
        ToolDetection::NotConfigured | ToolDetection::NotInstalled { .. } => None,
    }
}

/// Check if any of the given config file names exist in the project root.
fn has_project_config(project_root: Option<&Path>, filenames: &[&str]) -> bool {
    let root = match project_root {
        Some(r) => r,
        None => return false,
    };
    filenames.iter().any(|f| root.join(f).exists())
}

/// Check if pyproject.toml exists and contains a `[tool.<name>]` section.
fn has_pyproject_tool(project_root: Option<&Path>, tool_name: &str) -> bool {
    let root = match project_root {
        Some(r) => r,
        None => return false,
    };
    let pyproject = root.join("pyproject.toml");
    if !pyproject.exists() {
        return false;
    }
    match std::fs::read_to_string(&pyproject) {
        Ok(content) => {
            let pattern = format!("[tool.{}]", tool_name);
            content.contains(&pattern)
        }
        Err(_) => false,
    }
}

/// Detect whether a non-zero formatter exit was caused by the formatter
/// intentionally excluding the path (per its own config) rather than an
/// actual formatter or input error.
///
/// The patterns below come from real stderr output observed during
/// dogfooding. They're intentionally substring-based and case-insensitive
/// so minor formatter version differences in wording don't bypass the
/// check. Each pattern corresponds to a specific formatter's exclusion
/// signal:
/// - biome: `"No files were processed in the specified paths."`,
///   `"ignored by the configuration"`
/// - oxfmt: `"Expected at least one target file"`,
///   `"No files found matching the given patterns"`
/// - prettier: `"No files matching the pattern were found"`
/// - ruff: `"No Python files found under the given path(s)"`
///
/// rustfmt and gofmt/goimports rarely scope-restrict and have no known
/// stable marker, so they're not detected here. They'll fall through to
/// the generic `"error"` reason — acceptable because they almost never
/// emit a path-exclusion exit in practice.
fn formatter_excluded_path(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("no files were processed")
        || s.contains("ignored by the configuration")
        || s.contains("expected at least one target file")
        || s.contains("no files found matching the given patterns")
        || s.contains("no files matching the pattern")
        || s.contains("no python files found")
}

/// Auto-format a file using the detected formatter for its language.
///
/// Returns `(formatted, skip_reason)`:
/// - `(true, None)` — file was successfully formatted
/// - `(false, Some(reason))` — formatting was skipped, reason explains why
///
/// Skip reasons:
/// - `"unsupported_language"` — language has no formatter support in AFT
/// - `"no_formatter_configured"` — `format_on_edit=false` or no formatter
///   detected for the language in the project
/// - `"formatter_not_installed"` — configured formatter binary missing on
///   PATH and not in project's `node_modules/.bin`
/// - `"formatter_excluded_path"` — formatter ran but refused to process this
///   path because the project formatter config (e.g. biome.json `files.includes`,
///   prettier `.prettierignore`) excludes it. NOT an error in AFT or the user's
///   formatter — the user told the formatter not to touch this path. Agents
///   should treat this as informational.
/// - `"timeout"` — formatter exceeded `formatter_timeout_secs`
/// - `"error"` — formatter exited non-zero with an unrecognized error
///   (likely a real bug in the user's input or the formatter itself)
pub fn auto_format(path: &Path, config: &Config) -> (bool, Option<String>) {
    // Check if formatting is disabled via plugin config
    if !config.format_on_edit {
        return (false, Some("no_formatter_configured".to_string()));
    }

    let lang = match detect_language(path) {
        Some(l) => l,
        None => {
            log::debug!("format: {} (skipped: unsupported_language)", path.display());
            return (false, Some("unsupported_language".to_string()));
        }
    };
    if !has_formatter_support(lang) {
        log::debug!("format: {} (skipped: unsupported_language)", path.display());
        return (false, Some("unsupported_language".to_string()));
    }

    let (cmd, args) = match detect_formatter_for_path(path, lang, config) {
        ToolDetection::Found(cmd, args) => (cmd, args),
        ToolDetection::NotConfigured => {
            log::debug!(
                "format: {} (skipped: no_formatter_configured)",
                path.display()
            );
            return (false, Some("no_formatter_configured".to_string()));
        }
        ToolDetection::NotInstalled { tool } => {
            crate::slog_warn!(
                "format: {} (skipped: formatter_not_installed: {})",
                path.display(),
                tool
            );
            return (false, Some("formatter_not_installed".to_string()));
        }
    };

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    // Run the formatter in the project root so tool-local config files
    // (biome.json, .prettierrc, rustfmt.toml, etc.) are discovered. The
    // type-checker path (`validate_full`) already does this via
    // `path.parent()`; formatters need the same treatment. Without it,
    // formatters silently fall back to built-in defaults when the aft
    // process CWD differs from the project root (audit #18).
    let working_dir = config.project_root.as_deref();

    match run_external_tool(&cmd, &arg_refs, working_dir, config.formatter_timeout_secs) {
        Ok(_) => {
            crate::slog_info!("format: {} ({})", path.display(), cmd);
            (true, None)
        }
        Err(FormatError::Timeout { .. }) => {
            crate::slog_warn!("format: {} (skipped: timeout)", path.display());
            (false, Some("timeout".to_string()))
        }
        Err(FormatError::NotFound { .. }) => {
            crate::slog_warn!(
                "format: {} (skipped: formatter_not_installed)",
                path.display()
            );
            (false, Some("formatter_not_installed".to_string()))
        }
        Err(FormatError::Failed { stderr, .. }) => {
            // Distinguish "formatter intentionally ignored this path" from
            // "formatter actually errored". Many formatters scope themselves
            // to a project subtree (biome.json `files.includes`, prettier
            // `.prettierignore`, ruff `[tool.ruff]` config) and exit non-zero
            // when invoked on a path outside that scope. From AFT's perspective
            // that's not an error — the user told the formatter not to touch
            // this path. But the previous code returned a generic `"error"`
            // skip reason and logged at `debug` (silent under default
            // RUST_LOG=info), so the agent had no signal that the file
            // landed unformatted. Detect the common stderr fingerprints and
            // return a distinct, surfaced skip reason.
            if formatter_excluded_path(&stderr) {
                crate::slog_info!(
                    "format: {} (skipped: formatter_excluded_path; stderr: {})",
                    path.display(),
                    stderr.lines().next().unwrap_or("").trim()
                );
                return (false, Some("formatter_excluded_path".to_string()));
            }
            crate::slog_warn!(
                "format: {} (skipped: error: {})",
                path.display(),
                stderr.lines().next().unwrap_or("unknown").trim()
            );
            (false, Some("error".to_string()))
        }
        Err(FormatError::UnsupportedLanguage) => {
            log::debug!("format: {} (skipped: unsupported_language)", path.display());
            (false, Some("unsupported_language".to_string()))
        }
    }
}

/// Spawn a subprocess and capture output regardless of exit code.
///
/// Unlike `run_external_tool`, this does NOT treat non-zero exit as an error —
/// type checkers return non-zero when they find issues, which is expected.
/// Returns `FormatError::NotFound` when the binary isn't on PATH, and
/// `FormatError::Timeout` if the deadline is exceeded.
pub fn run_external_tool_capture(
    command: &str,
    args: &[&str],
    working_dir: Option<&Path>,
    timeout_secs: u32,
) -> Result<ExternalToolResult, FormatError> {
    let mut cmd = Command::new(command);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    isolate_in_process_group(&mut cmd);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(FormatError::NotFound {
                tool: command.to_string(),
            });
        }
        Err(e) => {
            return Err(FormatError::Failed {
                tool: command.to_string(),
                stderr: e.to_string(),
            });
        }
    };

    let outcome = wait_with_timeout(child, command, timeout_secs)?;
    Ok(ExternalToolResult {
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        exit_code: outcome.status.code().unwrap_or(-1),
        truncated: outcome.truncated,
    })
}

// ============================================================================
// Type-checker validation (R017)
// ============================================================================

/// A structured error from a type checker.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ValidationError {
    pub line: u32,
    pub column: u32,
    pub message: String,
    pub severity: String,
}

/// Detect the appropriate type checker command and arguments for a file.
///
/// Returns `(command, args)` for the type checker. The `--noEmit` / equivalent
/// flags ensure no output files are produced.
///
/// Supported:
/// - TypeScript/JavaScript/TSX → `tsc --noEmit` (or `tsgo --noEmit` when explicitly configured)
/// - Python → `pyright`
/// - Rust → `cargo check`
/// - Go → `go vet`
pub fn detect_type_checker(
    path: &Path,
    lang: LangId,
    config: &Config,
) -> Option<(String, Vec<String>)> {
    match detect_checker_for_path(path, lang, config) {
        ToolDetection::Found(cmd, args) => Some((cmd, args)),
        ToolDetection::NotConfigured | ToolDetection::NotInstalled { .. } => None,
    }
}

/// Parse type checker output into structured validation errors.
///
/// Handles output formats from tsc, pyright (JSON), cargo check (JSON), and go vet.
/// Filters to errors related to the edited file where feasible.
pub fn parse_checker_output(
    stdout: &str,
    stderr: &str,
    file: &Path,
    checker: &str,
) -> Vec<ValidationError> {
    let checker_name = checker_executable_name(checker);
    match checker_name.as_str() {
        "npx" | "tsc" | "tsgo" => parse_tsc_output(stdout, stderr, file),
        "biome" => parse_biome_output(stdout, stderr, file),
        "pyright" => parse_pyright_output(stdout, file),
        "ruff" => parse_ruff_output(stdout, stderr, file),
        "cargo" => parse_cargo_output(stdout, stderr, file),
        "go" => parse_go_vet_output(stderr, file),
        "staticcheck" => parse_staticcheck_output(stdout, stderr, file),
        _ => Vec::new(),
    }
}

fn checker_executable_name(checker: &str) -> String {
    let name = checker
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(checker)
        .to_ascii_lowercase();

    for suffix in [".exe", ".cmd", ".bat", ".ps1"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }

    name
}

fn normalize_path_for_compare(path: &str) -> String {
    path.trim_start_matches("file://")
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn diagnostic_path_matches(file: &Path, diagnostic_file: &str) -> bool {
    if diagnostic_file.is_empty() {
        return true;
    }

    let file_str = normalize_path_for_compare(&file.to_string_lossy());
    let diagnostic_str = normalize_path_for_compare(diagnostic_file);
    file_str == diagnostic_str
        || file_str.ends_with(&diagnostic_str)
        || diagnostic_str.ends_with(&file_str)
}

fn line_column_for_byte_offset(source: &str, offset: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut column = 1u32;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

fn json_string_at<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

fn json_u32_at(value: &serde_json::Value, path: &[&str]) -> Option<u32> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_u64().map(|n| n as u32)
}

fn json_location_path(value: &serde_json::Value) -> Option<&str> {
    json_string_at(value, &["location", "path", "file"])
        .or_else(|| json_string_at(value, &["location", "path"]))
        .or_else(|| json_string_at(value, &["filename"]))
        .or_else(|| json_string_at(value, &["file"]))
}

fn diagnostic_message(value: &serde_json::Value) -> String {
    json_string_at(value, &["description"])
        .or_else(|| json_string_at(value, &["message"]))
        .or_else(|| json_string_at(value, &["text"]))
        .or_else(|| json_string_at(value, &["category"]))
        .unwrap_or("unknown error")
        .to_string()
}

/// Parse tsc output lines like: `path(line,col): error TSxxxx: message`
fn parse_tsc_output(stdout: &str, stderr: &str, file: &Path) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let file_str = file.to_string_lossy();
    // tsc writes diagnostics to stdout (with --pretty false)
    let combined = format!("{}{}", stdout, stderr);
    for line in combined.lines() {
        // Format: path(line,col): severity TSxxxx: message
        // or: path(line,col): severity: message
        if let Some((loc, rest)) = line.split_once("): ") {
            // Check if this error is for our file (compare filename part)
            let file_part = loc.split('(').next().unwrap_or("");
            if !file_str.ends_with(file_part)
                && !file_part.ends_with(&*file_str)
                && file_part != &*file_str
            {
                continue;
            }

            // Parse (line,col) from the location part
            let coords = loc.split('(').last().unwrap_or("");
            let parts: Vec<&str> = coords.split(',').collect();
            let line_num: u32 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let col_num: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

            // Parse severity and message
            let (severity, message) = if let Some(msg) = rest.strip_prefix("error ") {
                ("error".to_string(), msg.to_string())
            } else if let Some(msg) = rest.strip_prefix("warning ") {
                ("warning".to_string(), msg.to_string())
            } else {
                ("error".to_string(), rest.to_string())
            };

            errors.push(ValidationError {
                line: line_num,
                column: col_num,
                message,
                severity,
            });
        }
    }
    errors
}

fn parse_biome_output(stdout: &str, stderr: &str, file: &Path) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for output in [stdout, stderr] {
        let trimmed = output.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
            parse_biome_json_value(&json, file, &mut errors);
        }
    }
    errors
}

fn parse_biome_json_value(
    json: &serde_json::Value,
    file: &Path,
    errors: &mut Vec<ValidationError>,
) {
    let diagnostics: Vec<&serde_json::Value> = if let Some(diags) = json
        .get("diagnostics")
        .and_then(|diagnostics| diagnostics.as_array())
    {
        diags.iter().collect()
    } else if let Some(diags) = json.as_array() {
        diags.iter().collect()
    } else {
        Vec::new()
    };

    let source = std::fs::read_to_string(file).ok();
    for diag in diagnostics {
        if let Some(diag_file) = json_location_path(diag) {
            if !diagnostic_path_matches(file, diag_file) {
                continue;
            }
        }

        let (line, column) = biome_line_column(diag, source.as_deref());
        errors.push(ValidationError {
            line,
            column,
            message: diagnostic_message(diag),
            severity: diag
                .get("severity")
                .and_then(|severity| severity.as_str())
                .unwrap_or("error")
                .to_lowercase(),
        });
    }
}

fn biome_line_column(diag: &serde_json::Value, source: Option<&str>) -> (u32, u32) {
    if let Some(line) =
        json_u32_at(diag, &["location", "line"]).or_else(|| json_u32_at(diag, &["line"]))
    {
        let column = json_u32_at(diag, &["location", "column"])
            .or_else(|| json_u32_at(diag, &["column"]))
            .unwrap_or(0);
        return (line, column);
    }

    let offset = diag
        .get("location")
        .and_then(|location| location.get("span"))
        .and_then(|span| span.as_array())
        .and_then(|span| span.first())
        .and_then(|offset| offset.as_u64())
        .map(|offset| offset as usize);

    match (source, offset) {
        (Some(source), Some(offset)) => line_column_for_byte_offset(source, offset),
        _ => (0, 0),
    }
}

fn parse_ruff_output(stdout: &str, stderr: &str, file: &Path) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for output in [stdout, stderr] {
        let trimmed = output.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
            parse_ruff_json_value(&json, file, &mut errors);
        }
    }
    errors
}

fn parse_ruff_json_value(json: &serde_json::Value, file: &Path, errors: &mut Vec<ValidationError>) {
    let diagnostics: Vec<&serde_json::Value> = if let Some(diags) = json.as_array() {
        diags.iter().collect()
    } else if let Some(diags) = json.get("diagnostics").and_then(|d| d.as_array()) {
        diags.iter().collect()
    } else {
        Vec::new()
    };

    for diag in diagnostics {
        let diag_file = diag
            .get("filename")
            .and_then(|filename| filename.as_str())
            .unwrap_or("");
        if !diagnostic_path_matches(file, diag_file) {
            continue;
        }

        let message = match (
            diag.get("code").and_then(|code| code.as_str()),
            diag.get("message").and_then(|message| message.as_str()),
        ) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            (None, Some(message)) => message.to_string(),
            (Some(code), None) => code.to_string(),
            (None, None) => "unknown error".to_string(),
        };

        errors.push(ValidationError {
            line: json_u32_at(diag, &["location", "row"])
                .or_else(|| json_u32_at(diag, &["location", "line"]))
                .unwrap_or(0),
            column: json_u32_at(diag, &["location", "column"]).unwrap_or(0),
            message,
            severity: diag
                .get("severity")
                .and_then(|severity| severity.as_str())
                .unwrap_or("error")
                .to_lowercase(),
        });
    }
}

/// Parse pyright JSON output.
fn parse_pyright_output(stdout: &str, file: &Path) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    // pyright --outputjson emits JSON with generalDiagnostics array
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(stdout) {
        if let Some(diags) = json.get("generalDiagnostics").and_then(|d| d.as_array()) {
            for diag in diags {
                // Filter to our file
                let diag_file = diag.get("file").and_then(|f| f.as_str()).unwrap_or("");
                if !diagnostic_path_matches(file, diag_file) {
                    continue;
                }

                let line_num = diag
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.get("line"))
                    .and_then(|l| l.as_u64())
                    .unwrap_or(0) as u32;
                let col_num = diag
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.get("character"))
                    .and_then(|c| c.as_u64())
                    .unwrap_or(0) as u32;
                let message = diag
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                let severity = diag
                    .get("severity")
                    .and_then(|s| s.as_str())
                    .unwrap_or("error")
                    .to_lowercase();

                errors.push(ValidationError {
                    line: line_num + 1,  // pyright uses 0-indexed lines
                    column: col_num + 1, // pyright uses 0-indexed columns
                    message,
                    severity,
                });
            }
        }
    }
    errors
}

/// Parse cargo check JSON output, filtering to errors in the target file.
fn parse_cargo_output(stdout: &str, _stderr: &str, file: &Path) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let file_str = file.to_string_lossy();

    for line in stdout.lines() {
        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) {
            if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
                continue;
            }
            let message_obj = match msg.get("message") {
                Some(m) => m,
                None => continue,
            };

            let level = message_obj
                .get("level")
                .and_then(|l| l.as_str())
                .unwrap_or("error");

            // Only include errors and warnings, skip notes/help
            if level != "error" && level != "warning" {
                continue;
            }

            let text = message_obj
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();

            // Find the primary span for our file
            if let Some(spans) = message_obj.get("spans").and_then(|s| s.as_array()) {
                for span in spans {
                    let span_file = span.get("file_name").and_then(|f| f.as_str()).unwrap_or("");
                    let is_primary = span
                        .get("is_primary")
                        .and_then(|p| p.as_bool())
                        .unwrap_or(false);

                    if !is_primary {
                        continue;
                    }

                    // Filter to our file
                    if !file_str.ends_with(span_file)
                        && !span_file.ends_with(&*file_str)
                        && span_file != &*file_str
                    {
                        continue;
                    }

                    let line_num =
                        span.get("line_start").and_then(|l| l.as_u64()).unwrap_or(0) as u32;
                    let col_num = span
                        .get("column_start")
                        .and_then(|c| c.as_u64())
                        .unwrap_or(0) as u32;

                    errors.push(ValidationError {
                        line: line_num,
                        column: col_num,
                        message: text.clone(),
                        severity: level.to_string(),
                    });
                }
            }
        }
    }
    errors
}

/// Parse go vet output lines like: `path:line:col: message`
fn parse_go_vet_output(stderr: &str, file: &Path) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let pattern =
        regex::Regex::new(r"^(?P<file>.+?):(?P<line>\d+)(?::(?P<col>\d+))?:\s*(?P<message>.*)$")
            .expect("valid go vet diagnostic regex");

    for line in stderr.lines() {
        let Some(captures) = pattern.captures(line) else {
            continue;
        };

        let err_file = captures
            .name("file")
            .map(|m| m.as_str())
            .unwrap_or("")
            .trim();
        if !diagnostic_path_matches(file, err_file) {
            continue;
        }

        errors.push(ValidationError {
            line: captures
                .name("line")
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0),
            column: captures
                .name("col")
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0),
            message: captures
                .name("message")
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_else(|| "unknown error".to_string()),
            severity: "error".to_string(),
        });
    }
    errors
}

fn parse_staticcheck_output(stdout: &str, stderr: &str, file: &Path) -> Vec<ValidationError> {
    let combined = format!("{}\n{}", stdout, stderr);
    let trimmed = combined.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut errors = Vec::new();
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
        parse_staticcheck_json_value(&json, file, &mut errors);
        return errors;
    }

    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            parse_staticcheck_json_value(&json, file, &mut errors);
        }
    }

    errors
}

fn parse_staticcheck_json_value(
    json: &serde_json::Value,
    file: &Path,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(diags) = json.as_array() {
        for diag in diags {
            parse_staticcheck_diag(diag, file, errors);
        }
    } else if let Some(diags) = json.get("diagnostics").and_then(|d| d.as_array()) {
        for diag in diags {
            parse_staticcheck_diag(diag, file, errors);
        }
    } else if let Some(diags) = json.get("issues").and_then(|d| d.as_array()) {
        for diag in diags {
            parse_staticcheck_diag(diag, file, errors);
        }
    } else {
        parse_staticcheck_diag(json, file, errors);
    }
}

fn parse_staticcheck_diag(
    diag: &serde_json::Value,
    file: &Path,
    errors: &mut Vec<ValidationError>,
) {
    let diag_file = json_string_at(diag, &["location", "file"])
        .or_else(|| json_string_at(diag, &["file"]))
        .unwrap_or("");
    if !diagnostic_path_matches(file, diag_file) {
        return;
    }

    let message = match (
        diag.get("code").and_then(|code| code.as_str()),
        diag.get("message").and_then(|message| message.as_str()),
    ) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (None, Some(message)) => message.to_string(),
        (Some(code), None) => code.to_string(),
        (None, None) => "unknown error".to_string(),
    };

    errors.push(ValidationError {
        line: json_u32_at(diag, &["location", "line"])
            .or_else(|| json_u32_at(diag, &["line"]))
            .unwrap_or(0),
        column: json_u32_at(diag, &["location", "column"])
            .or_else(|| json_u32_at(diag, &["column"]))
            .unwrap_or(0),
        message,
        severity: diag
            .get("severity")
            .and_then(|severity| severity.as_str())
            .unwrap_or("error")
            .to_lowercase(),
    });
}

fn output_tail_summary(stdout: &str, stderr: &str, truncated: bool) -> String {
    let mut parts = Vec::new();
    if let Some(tail) = short_output_tail(stderr) {
        parts.push(format!("stderr: {tail}"));
    }
    if let Some(tail) = short_output_tail(stdout) {
        parts.push(format!("stdout: {tail}"));
    }
    if truncated {
        parts.push("output truncated".to_string());
    }

    if parts.is_empty() {
        "no output".to_string()
    } else {
        parts.join("; ")
    }
}

fn short_output_tail(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut lines: Vec<&str> = trimmed.lines().rev().take(3).collect();
    lines.reverse();
    let mut tail = lines.join(" | ");
    const MAX_TAIL_CHARS: usize = 500;
    if tail.len() > MAX_TAIL_CHARS {
        let start = tail.len().saturating_sub(MAX_TAIL_CHARS);
        tail = format!("…{}", &tail[start..]);
    }
    Some(tail)
}

/// Run the project's type checker and return structured validation errors.
///
/// Returns `(errors, skip_reason)`:
/// - `(errors, None)` — checker ran, errors may be empty (= valid code)
/// - `([], Some(reason))` — checker was skipped
///
/// Skip reasons: `"unsupported_language"`, `"no_checker_configured"`,
/// `"checker_not_installed"`, `"timeout"`, `"error"`
pub fn validate_full(path: &Path, config: &Config) -> (Vec<ValidationError>, Option<String>) {
    let lang = match detect_language(path) {
        Some(l) => l,
        None => {
            log::debug!(
                "validate: {} (skipped: unsupported_language)",
                path.display()
            );
            return (Vec::new(), Some("unsupported_language".to_string()));
        }
    };
    if !has_checker_support(lang) {
        log::debug!(
            "validate: {} (skipped: unsupported_language)",
            path.display()
        );
        return (Vec::new(), Some("unsupported_language".to_string()));
    }

    let (cmd, args) = match detect_checker_for_path(path, lang, config) {
        ToolDetection::Found(cmd, args) => (cmd, args),
        ToolDetection::NotConfigured => {
            log::debug!(
                "validate: {} (skipped: no_checker_configured)",
                path.display()
            );
            return (Vec::new(), Some("no_checker_configured".to_string()));
        }
        ToolDetection::NotInstalled { tool } => {
            crate::slog_warn!(
                "validate: {} (skipped: checker_not_installed: {})",
                path.display(),
                tool
            );
            return (Vec::new(), Some("checker_not_installed".to_string()));
        }
    };

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    // Type checkers may need to run from the project root
    let working_dir = config.project_root.as_deref();

    match run_external_tool_capture(
        &cmd,
        &arg_refs,
        working_dir,
        config.type_checker_timeout_secs,
    ) {
        Ok(result) => {
            let errors = parse_checker_output(&result.stdout, &result.stderr, path, &cmd);
            if result.exit_code != 0 && errors.is_empty() {
                let summary = output_tail_summary(&result.stdout, &result.stderr, result.truncated);
                log::debug!(
                    "validate: {} (skipped: error: checker exited {} with {})",
                    path.display(),
                    result.exit_code,
                    summary
                );
                return (Vec::new(), Some("error".to_string()));
            }
            log::debug!(
                "validate: {} ({}, {} errors)",
                path.display(),
                cmd,
                errors.len()
            );
            (errors, None)
        }
        Err(FormatError::Timeout { .. }) => {
            crate::slog_error!("validate: {} (skipped: timeout)", path.display());
            (Vec::new(), Some("timeout".to_string()))
        }
        Err(FormatError::NotFound { .. }) => {
            crate::slog_warn!(
                "validate: {} (skipped: checker_not_installed)",
                path.display()
            );
            (Vec::new(), Some("checker_not_installed".to_string()))
        }
        Err(FormatError::Failed { stderr, .. }) => {
            log::debug!(
                "validate: {} (skipped: error: {})",
                path.display(),
                stderr.lines().next().unwrap_or("unknown")
            );
            (Vec::new(), Some("error".to_string()))
        }
        Err(FormatError::UnsupportedLanguage) => {
            log::debug!(
                "validate: {} (skipped: unsupported_language)",
                path.display()
            );
            (Vec::new(), Some("unsupported_language".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Serializes tests that mutate the global TOOL_RESOLUTION_CACHE /
    /// TOOL_AVAILABILITY_CACHE. Cargo runs tests in parallel by default, and
    /// `clear_tool_cache()` from one test would otherwise wipe cached entries
    /// that another test had just written, causing flaky CI failures (the
    /// `resolve_tool_caches_negative_result_until_clear` failure on Linux
    /// runners had exactly this shape).
    fn tool_cache_test_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let mutex = LOCK.get_or_init(|| Mutex::new(()));
        // Recover from poisoning so a panic in one test doesn't permanently
        // wedge the rest of the suite.
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[test]
    fn run_external_tool_not_found() {
        let result = run_external_tool("__nonexistent_tool_xyz__", &[], None, 5);
        assert!(result.is_err());
        match result.unwrap_err() {
            FormatError::NotFound { tool } => {
                assert_eq!(tool, "__nonexistent_tool_xyz__");
            }
            other => panic!("expected NotFound, got: {:?}", other),
        }
    }

    #[test]
    fn run_external_tool_timeout_kills_subprocess() {
        // Use `sleep 60` as a long-running process, timeout after 1 second
        let result = run_external_tool("sleep", &["60"], None, 1);
        assert!(result.is_err());
        match result.unwrap_err() {
            FormatError::Timeout { tool, timeout_secs } => {
                assert_eq!(tool, "sleep");
                assert_eq!(timeout_secs, 1);
            }
            other => panic!("expected Timeout, got: {:?}", other),
        }
    }

    #[test]
    fn run_external_tool_success() {
        let result = run_external_tool("echo", &["hello"], None, 5);
        assert!(result.is_ok());
        let res = result.unwrap();
        assert_eq!(res.exit_code, 0);
        assert!(res.stdout.contains("hello"));
    }

    #[cfg(unix)]
    #[test]
    fn format_helper_handles_large_stderr_without_deadlock() {
        let start = Instant::now();
        let result = run_external_tool_capture(
            "sh",
            &[
                "-c",
                "i=0; while [ $i -lt 1024 ]; do printf '%1024s\\n' x >&2; i=$((i+1)); done",
            ],
            None,
            2,
        )
        .expect("large stderr command should complete");

        assert_eq!(result.exit_code, 0);
        assert!(
            result.stderr.len() >= 1024 * 1024,
            "expected full stderr capture, got {} bytes",
            result.stderr.len()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn run_external_tool_nonzero_exit() {
        // `false` always exits with code 1
        let result = run_external_tool("false", &[], None, 5);
        assert!(result.is_err());
        match result.unwrap_err() {
            FormatError::Failed { tool, .. } => {
                assert_eq!(tool, "false");
            }
            other => panic!("expected Failed, got: {:?}", other),
        }
    }

    #[test]
    fn path_lookup_probe_args_match_go_tool_conventions() {
        assert_eq!(path_lookup_probe_args("go"), &["version"]);
        assert!(path_lookup_probe_args("gofmt").is_empty());
        assert_eq!(path_lookup_probe_args("rustfmt"), &["--version"]);
    }

    #[test]
    fn auto_format_unsupported_language() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "hello").unwrap();

        let config = Config::default();
        let (formatted, reason) = auto_format(&path, &config);
        assert!(!formatted);
        assert_eq!(reason.as_deref(), Some("unsupported_language"));
    }

    #[test]
    fn detect_formatter_rust_when_rustfmt_available() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        let path = dir.path().join("test.rs");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let result = detect_formatter(&path, LangId::Rust, &config);
        if resolve_tool("rustfmt", config.project_root.as_deref()).is_some() {
            let (cmd, args) = result.unwrap();
            // Windows resolves to `rustfmt.exe` and may include a full path
            // (e.g. `C:\Users\...\.cargo\bin\rustfmt.exe`). Just require the
            // command stem to be `rustfmt`.
            let stem = std::path::Path::new(&cmd)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            assert_eq!(stem, "rustfmt", "expected rustfmt, got {cmd}");
            assert!(args.iter().any(|a| a.ends_with("test.rs")));
        } else {
            assert!(result.is_none());
        }
    }

    #[test]
    fn detect_formatter_go_mapping() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module test\ngo 1.21").unwrap();
        let path = dir.path().join("main.go");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let result = detect_formatter(&path, LangId::Go, &config);
        if resolve_tool("goimports", config.project_root.as_deref()).is_some() {
            let (cmd, args) = result.unwrap();
            assert_eq!(
                std::path::Path::new(&cmd)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(""),
                "goimports",
                "expected goimports, got {cmd}"
            );
            assert!(args.contains(&"-w".to_string()));
        } else if resolve_tool("gofmt", config.project_root.as_deref()).is_some() {
            let (cmd, args) = result.unwrap();
            assert_eq!(
                std::path::Path::new(&cmd)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(""),
                "gofmt",
                "expected gofmt, got {cmd}"
            );
            assert!(args.contains(&"-w".to_string()));
        } else {
            assert!(result.is_none());
        }
    }

    #[test]
    fn detect_formatter_python_mapping() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ruff.toml"), "").unwrap();
        let path = dir.path().join("main.py");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let result = detect_formatter(&path, LangId::Python, &config);
        if ruff_format_available(config.project_root.as_deref()) {
            let (cmd, args) = result.unwrap();
            assert_eq!(
                std::path::Path::new(&cmd)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(""),
                "ruff",
                "expected ruff, got {cmd}"
            );
            assert!(args.contains(&"format".to_string()));
        } else {
            assert!(result.is_none());
        }
    }

    #[test]
    fn detect_formatter_no_config_returns_none() {
        let path = Path::new("test.ts");
        let result = detect_formatter(path, LangId::TypeScript, &Config::default());
        assert!(
            result.is_none(),
            "expected no formatter without project config"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detect_formatter_oxfmt_config_for_typescript_projects() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".oxfmtrc.json"), "{}\n").unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake = bin_dir.join("oxfmt");
        fs::write(&fake, "#!/bin/sh\necho 1.0.0").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let path = dir.path().join("src/app.ts");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };

        let (cmd, args) = detect_formatter(&path, LangId::TypeScript, &config).unwrap();
        assert!(cmd.ends_with("oxfmt"), "expected oxfmt, got {cmd}");
        assert_eq!(args[0], "--write");
        assert!(args.iter().any(|arg| arg.ends_with("src/app.ts")));
    }

    // Unix-only: `resolve_tool_uncached` checks `node_modules/.bin/<name>`
    // without trying Windows extensions (.cmd/.exe/.bat). Writing
    // `biome.cmd` would not be found by the resolver. A future product
    // fix could extend resolve_tool to honor PATHEXT; for now this test
    // focuses on the explicit-override semantics on Unix.
    #[cfg(unix)]
    #[test]
    fn detect_formatter_explicit_override() {
        // Create a temp dir with a fake node_modules/.bin/biome so resolve_tool finds it
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let fake = bin_dir.join("biome");
        fs::write(&fake, "#!/bin/sh\necho 1.0.0").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let path = Path::new("test.ts");
        let mut config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        config
            .formatter
            .insert("typescript".to_string(), "biome".to_string());
        let result = detect_formatter(path, LangId::TypeScript, &config);
        let (cmd, args) = result.unwrap();
        assert!(cmd.contains("biome"), "expected biome in cmd, got: {}", cmd);
        assert!(args.contains(&"format".to_string()));
        assert!(args.contains(&"--write".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn detect_formatter_explicit_oxfmt_override() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let fake = bin_dir.join("oxfmt");
        fs::write(&fake, "#!/bin/sh\necho 1.0.0").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let path = Path::new("test.ts");
        let mut config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        config
            .formatter
            .insert("typescript".to_string(), "oxfmt".to_string());

        let (cmd, args) = detect_formatter(path, LangId::TypeScript, &config).unwrap();
        assert!(cmd.contains("oxfmt"), "expected oxfmt in cmd, got: {cmd}");
        assert_eq!(args, vec!["--write".to_string(), "test.ts".to_string()]);
    }

    #[test]
    fn resolve_tool_caches_positive_result_until_clear() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let tool = bin_dir.join("aft-cache-hit-tool");
        fs::write(&tool, "#!/bin/sh\necho cached").unwrap();

        let first = resolve_tool("aft-cache-hit-tool", Some(dir.path()));
        assert_eq!(first.as_deref(), Some(tool.to_string_lossy().as_ref()));

        fs::remove_file(&tool).unwrap();
        let cached = resolve_tool("aft-cache-hit-tool", Some(dir.path()));
        assert_eq!(cached, first);

        clear_tool_cache();
        assert!(resolve_tool("aft-cache-hit-tool", Some(dir.path())).is_none());
    }

    #[test]
    fn resolve_tool_caches_negative_result_until_clear() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        let tool = bin_dir.join("aft-cache-miss-tool");

        assert!(resolve_tool("aft-cache-miss-tool", Some(dir.path())).is_none());

        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(&tool, "#!/bin/sh\necho cached").unwrap();
        assert!(resolve_tool("aft-cache-miss-tool", Some(dir.path())).is_none());

        clear_tool_cache();
        assert_eq!(
            resolve_tool("aft-cache-miss-tool", Some(dir.path())).as_deref(),
            Some(tool.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn auto_format_happy_path_rustfmt() {
        if resolve_tool("rustfmt", None).is_none() {
            crate::slog_warn!("skipping: rustfmt not available");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        let path = dir.path().join("test.rs");

        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "fn    main()   {{  println!(\"hello\");  }}").unwrap();
        drop(f);

        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let (formatted, reason) = auto_format(&path, &config);
        assert!(formatted, "expected formatting to succeed");
        assert!(reason.is_none());

        let content = fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("fn    main"),
            "expected rustfmt to fix spacing"
        );
    }

    #[test]
    fn formatter_excluded_path_detects_biome_messages() {
        // Real biome 1.x output when invoked on a path outside files.includes.
        let stderr = "format ━━━━━━━━━━━━━━━━━\n\n  × No files were processed in the specified paths.\n\n  i Check your biome.json or biome.jsonc to ensure the paths are not ignored by the configuration.\n";
        assert!(
            formatter_excluded_path(stderr),
            "expected biome exclusion stderr to be detected"
        );
    }

    #[test]
    fn formatter_excluded_path_detects_prettier_messages() {
        // Real prettier output when given a glob/path that resolves to nothing
        // it's allowed to format (after .prettierignore filtering).
        let stderr = "[error] No files matching the pattern were found: \"src/scratch.ts\".\n";
        assert!(
            formatter_excluded_path(stderr),
            "expected prettier exclusion stderr to be detected"
        );
    }

    #[test]
    fn formatter_excluded_path_detects_oxfmt_messages() {
        assert!(formatter_excluded_path(
            "Expected at least one target file. All matched files may have been excluded by ignore rules."
        ));
        assert!(formatter_excluded_path(
            "No files found matching the given patterns."
        ));
    }

    #[test]
    fn formatter_excluded_path_detects_ruff_messages() {
        // Real ruff output when invoked outside its [tool.ruff] scope.
        let stderr = "warning: No Python files found under the given path(s).\n";
        assert!(
            formatter_excluded_path(stderr),
            "expected ruff exclusion stderr to be detected"
        );
    }

    #[test]
    fn formatter_excluded_path_is_case_insensitive() {
        assert!(formatter_excluded_path("NO FILES WERE PROCESSED"));
        assert!(formatter_excluded_path("Ignored By The Configuration"));
        assert!(formatter_excluded_path("EXPECTED AT LEAST ONE TARGET FILE"));
    }

    #[test]
    fn formatter_excluded_path_rejects_real_errors() {
        // Counter-cases: actual formatter errors must NOT be treated as
        // exclusion. This guards against the detection being too greedy.
        assert!(!formatter_excluded_path(""));
        assert!(!formatter_excluded_path("syntax error: unexpected token"));
        assert!(!formatter_excluded_path("formatter crashed: out of memory"));
        assert!(!formatter_excluded_path(
            "permission denied: /readonly/file"
        ));
        assert!(!formatter_excluded_path(
            "biome internal error: please report"
        ));
    }

    #[test]
    fn parse_tsc_output_basic() {
        let stdout = "src/app.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.\nsrc/app.ts(20,1): error TS2304: Cannot find name 'foo'.\n";
        let file = Path::new("src/app.ts");
        let errors = parse_tsc_output(stdout, "", file);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].line, 10);
        assert_eq!(errors[0].column, 5);
        assert_eq!(errors[0].severity, "error");
        assert!(errors[0].message.contains("TS2322"));
        assert_eq!(errors[1].line, 20);
    }

    #[test]
    fn parse_tsc_output_filters_other_files() {
        let stdout =
            "other.ts(1,1): error TS2322: wrong file\nsrc/app.ts(5,3): error TS1234: our file\n";
        let file = Path::new("src/app.ts");
        let errors = parse_tsc_output(stdout, "", file);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 5);
    }

    #[test]
    fn parse_cargo_output_basic() {
        let json_line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","spans":[{"file_name":"src/main.rs","line_start":10,"column_start":5,"is_primary":true}]}}"#;
        let file = Path::new("src/main.rs");
        let errors = parse_cargo_output(json_line, "", file);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 10);
        assert_eq!(errors[0].column, 5);
        assert_eq!(errors[0].severity, "error");
        assert!(errors[0].message.contains("mismatched types"));
    }

    #[test]
    fn parse_cargo_output_skips_notes() {
        // Notes and help messages should be filtered out
        let json_line = r#"{"reason":"compiler-message","message":{"level":"note","message":"expected this","spans":[{"file_name":"src/main.rs","line_start":10,"column_start":5,"is_primary":true}]}}"#;
        let file = Path::new("src/main.rs");
        let errors = parse_cargo_output(json_line, "", file);
        assert_eq!(errors.len(), 0);
    }

    #[test]
    fn parse_cargo_output_filters_other_files() {
        let json_line = r#"{"reason":"compiler-message","message":{"level":"error","message":"err","spans":[{"file_name":"src/other.rs","line_start":1,"column_start":1,"is_primary":true}]}}"#;
        let file = Path::new("src/main.rs");
        let errors = parse_cargo_output(json_line, "", file);
        assert_eq!(errors.len(), 0);
    }

    #[test]
    fn parse_go_vet_output_basic() {
        let stderr = "main.go:10:5: unreachable code\nmain.go:20: another issue\n";
        let file = Path::new("main.go");
        let errors = parse_go_vet_output(stderr, file);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].line, 10);
        assert_eq!(errors[0].column, 5);
        assert!(errors[0].message.contains("unreachable code"));
        assert_eq!(errors[1].line, 20);
        assert_eq!(errors[1].column, 0);
    }

    #[test]
    fn parse_pyright_output_basic() {
        let stdout = r#"{"generalDiagnostics":[{"file":"test.py","range":{"start":{"line":4,"character":10}},"message":"Type error here","severity":"error"}]}"#;
        let file = Path::new("test.py");
        let errors = parse_pyright_output(stdout, file);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 5); // 0-indexed → 1-indexed
        assert_eq!(errors[0].column, 11);
        assert_eq!(errors[0].severity, "error");
        assert!(errors[0].message.contains("Type error here"));
    }

    #[test]
    fn validate_full_unsupported_language() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "hello").unwrap();

        let config = Config::default();
        let (errors, reason) = validate_full(&path, &config);
        assert!(errors.is_empty());
        assert_eq!(reason.as_deref(), Some("unsupported_language"));
    }

    #[test]
    fn detect_type_checker_rust() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        let path = dir.path().join("src/main.rs");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let result = detect_type_checker(&path, LangId::Rust, &config);
        if resolve_tool("cargo", config.project_root.as_deref()).is_some() {
            let (cmd, args) = result.unwrap();
            assert_eq!(
                std::path::Path::new(&cmd)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(""),
                "cargo",
                "expected cargo, got {cmd}"
            );
            assert!(args.contains(&"check".to_string()));
        } else {
            assert!(result.is_none());
        }
    }

    #[test]
    fn detect_type_checker_go() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module test\ngo 1.21").unwrap();
        let path = dir.path().join("main.go");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let result = detect_type_checker(&path, LangId::Go, &config);
        if resolve_tool("go", config.project_root.as_deref()).is_some() {
            let (cmd, _args) = result.unwrap();
            // Could be staticcheck or go vet depending on what's installed
            assert!(cmd == "go" || cmd == "staticcheck");
        } else {
            assert!(result.is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn detect_type_checker_defaults_to_tsc_for_typescript() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let fake_tsc = bin_dir.join("tsc");
        fs::write(&fake_tsc, "#!/bin/sh\nexit 0").unwrap();
        fs::set_permissions(&fake_tsc, fs::Permissions::from_mode(0o755)).unwrap();
        let fake_tsgo = bin_dir.join("tsgo");
        fs::write(&fake_tsgo, "#!/bin/sh\nexit 0").unwrap();
        fs::set_permissions(&fake_tsgo, fs::Permissions::from_mode(0o755)).unwrap();

        let path = dir.path().join("src/app.ts");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };

        let (cmd, args) = detect_type_checker(&path, LangId::TypeScript, &config).unwrap();
        assert!(cmd.ends_with("tsc"), "expected tsc by default, got: {cmd}");
        assert_eq!(args, vec!["--noEmit", "--pretty", "false"]);
    }

    #[cfg(unix)]
    #[test]
    fn detect_type_checker_uses_tsgo_when_explicitly_configured() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let fake_tsgo = bin_dir.join("tsgo");
        fs::write(&fake_tsgo, "#!/bin/sh\nexit 0").unwrap();
        fs::set_permissions(&fake_tsgo, fs::Permissions::from_mode(0o755)).unwrap();

        let path = dir.path().join("src/app.ts");
        let mut config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        config
            .checker
            .insert("typescript".to_string(), "tsgo".to_string());

        let (cmd, args) = detect_type_checker(&path, LangId::TypeScript, &config).unwrap();
        assert!(cmd.ends_with("tsgo"), "expected tsgo, got: {cmd}");
        assert_eq!(args, vec!["--noEmit", "--pretty", "false"]);
    }

    #[cfg(unix)]
    #[test]
    fn validate_full_explicit_tsgo_parses_diagnostics() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let path = src_dir.join("app.ts");
        fs::write(&path, "const value: number = 'nope';\n").unwrap();

        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let fake_tsgo = bin_dir.join("tsgo");
        fs::write(
            &fake_tsgo,
            "#!/bin/sh\nif [ \"$1 $2 $3\" != \"--noEmit --pretty false\" ]; then echo \"bad args: $*\" >&2; exit 3; fi\nprintf '%s\n' \"src/app.ts(1,23): error TS2322: Type 'string' is not assignable to type 'number'.\"\nexit 2\n",
        )
        .unwrap();
        fs::set_permissions(&fake_tsgo, fs::Permissions::from_mode(0o755)).unwrap();

        let mut config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        config
            .checker
            .insert("typescript".to_string(), "tsgo".to_string());

        let (errors, reason) = validate_full(&path, &config);
        assert_eq!(reason, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 1);
        assert_eq!(errors[0].column, 23);
        assert!(errors[0].message.contains("TS2322"));
    }

    #[test]
    fn run_external_tool_capture_nonzero_not_error() {
        // `false` exits with code 1 — capture should still return Ok
        let result = run_external_tool_capture("false", &[], None, 5);
        assert!(result.is_ok(), "capture should not error on non-zero exit");
        assert_eq!(result.unwrap().exit_code, 1);
    }

    #[test]
    fn run_external_tool_capture_not_found() {
        let result = run_external_tool_capture("__nonexistent_xyz__", &[], None, 5);
        assert!(result.is_err());
        match result.unwrap_err() {
            FormatError::NotFound { tool } => assert_eq!(tool, "__nonexistent_xyz__"),
            other => panic!("expected NotFound, got: {:?}", other),
        }
    }

    // GitHub issue #47: GUI-launched editors miss /opt/homebrew/bin etc. from
    // PATH. `try_well_known_path_lookup` should find the tool at well-known
    // install locations even when PATH wouldn't.
    #[cfg(unix)]
    #[test]
    fn well_known_search_paths_include_homebrew_cargo_go_and_local() {
        let home = std::ffi::OsString::from("/Users/test-home");
        let paths = well_known_search_paths("toolx", Some(&home));
        let strs: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        // Order matters: Homebrew prefixes come first so an installed-via-brew
        // tool wins over a HOME-rooted shim.
        assert_eq!(strs[0], "/opt/homebrew/bin/toolx");
        assert_eq!(strs[1], "/usr/local/bin/toolx");
        assert_eq!(strs[2], "/usr/local/go/bin/toolx");
        assert_eq!(strs[3], "/usr/bin/toolx");
        assert_eq!(strs[4], "/snap/bin/toolx");
        assert_eq!(strs[5], "/Users/test-home/.cargo/bin/toolx");
        assert_eq!(strs[6], "/Users/test-home/go/bin/toolx");
        assert_eq!(strs[7], "/Users/test-home/.local/bin/toolx");
        assert_eq!(strs.len(), 8);
    }

    #[cfg(unix)]
    #[test]
    fn well_known_search_paths_skips_home_when_unset() {
        let paths = well_known_search_paths("toolx", None);
        assert_eq!(paths.len(), 5);
        assert!(paths[0].ends_with("opt/homebrew/bin/toolx"));
        assert!(paths[1].ends_with("usr/local/bin/toolx"));
        assert!(paths[2].ends_with("usr/local/go/bin/toolx"));
        assert!(paths[3].ends_with("usr/bin/toolx"));
        assert!(paths[4].ends_with("snap/bin/toolx"));
    }

    #[cfg(unix)]
    #[test]
    fn try_well_known_path_lookup_in_finds_executable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let tool_path = bin_dir.join("toolx");
        fs::write(&tool_path, "#!/bin/sh\necho test").unwrap();
        let mut perms = fs::metadata(&tool_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tool_path, perms).unwrap();

        let candidates = vec![
            dir.path().join("missing/toolx"),
            tool_path.clone(),
            dir.path().join("alt/toolx"),
        ];
        let found = try_well_known_path_lookup_in(&candidates);
        assert_eq!(found, Some(tool_path));
    }

    #[cfg(unix)]
    #[test]
    fn try_well_known_path_lookup_in_skips_non_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        // File exists but is not marked executable (default 0o644 on most umasks).
        let tool_path = bin_dir.join("toolx");
        fs::write(&tool_path, "not a real tool").unwrap();

        let found = try_well_known_path_lookup_in(&std::slice::from_ref(&tool_path));
        assert!(found.is_none(), "non-executable file should be skipped");
    }

    #[cfg(unix)]
    #[test]
    fn try_well_known_path_lookup_in_skips_directories_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        // A directory at the expected path should not count as a tool.
        let candidates = vec![dir.path().to_path_buf(), dir.path().join("does-not-exist")];
        assert!(try_well_known_path_lookup_in(&candidates).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn try_well_known_path_lookup_is_noop_on_windows() {
        // On Windows we deliberately skip POSIX well-known paths; only PATH
        // lookup applies. The public entry point should always return None.
        assert!(try_well_known_path_lookup("biome").is_none());
    }

    // GitHub issue #47: wording must not claim "but not installed" — the tool
    // may be installed but missing from AFT's PATH (GUI-launched editor).
    #[test]
    fn configured_tool_hint_does_not_claim_not_installed() {
        let hint = configured_tool_hint("biome", "biome.json");
        assert!(
            hint.contains("was not found on PATH or in common install locations"),
            "hint should explain the PATH miss: got {:?}",
            hint
        );
        assert!(
            !hint.contains("but not installed"),
            "hint must not claim the tool isn't installed: got {:?}",
            hint
        );
    }

    #[test]
    fn install_hint_for_go_mentions_path() {
        // Verify the Go-specific hint nudges users toward checking PATH
        // (Homebrew install location is the most common GUI-launch PATH miss).
        let hint = install_hint("go");
        assert!(
            hint.contains("PATH"),
            "go install hint should mention PATH: got {:?}",
            hint
        );
    }

    #[test]
    fn read_bounded_to_string_truncates_after_limit() {
        let (text, truncated) = read_bounded_to_string(std::io::Cursor::new(b"abcdef"), 4);
        assert_eq!(text, "abcd");
        assert!(truncated);

        let (text, truncated) = read_bounded_to_string(std::io::Cursor::new(b"abc"), 4);
        assert_eq!(text, "abc");
        assert!(!truncated);
    }

    #[test]
    fn windows_local_node_bin_extensions_follow_pathext_then_defaults() {
        let pathext = std::ffi::OsString::from(".EXE;.CMD;.BAT;.CMD");
        let extensions = windows_local_node_bin_extensions(Some(&pathext));
        assert_eq!(extensions, vec![".exe", ".cmd", ".bat", ".ps1"]);
    }

    #[test]
    fn checker_executable_name_strips_paths_and_windows_extensions() {
        assert_eq!(checker_executable_name("/usr/local/bin/ruff"), "ruff");
        assert_eq!(checker_executable_name(r"C:\Go\bin\go.exe"), "go");
        assert_eq!(
            checker_executable_name(r"C:\repo\node_modules\.bin\biome.cmd"),
            "biome"
        );
    }

    #[test]
    fn parse_biome_output_json_reporter() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src/app.ts");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "const value = 1;\nconsole.log(value);\n").unwrap();
        // Build the JSON via serde so the path is correctly escaped on Windows
        // (backslashes in paths would otherwise break a raw JSON string literal).
        let stdout = serde_json::json!({
            "diagnostics": [
                {
                    "severity": "warning",
                    "description": "Avoid console.log",
                    "location": {
                        "path": { "file": file.to_string_lossy() },
                        "span": [17, 28],
                    },
                },
            ],
        })
        .to_string();

        let errors = parse_biome_output(&stdout, "", &file);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 2);
        assert_eq!(errors[0].column, 1);
        assert_eq!(errors[0].severity, "warning");
        assert!(errors[0].message.contains("Avoid console.log"));
    }

    #[test]
    fn parse_ruff_output_json() {
        let stdout = r#"[{"filename":"pkg/main.py","location":{"row":3,"column":5},"code":"F401","message":"`os` imported but unused"}]"#;
        let errors = parse_ruff_output(stdout, "", Path::new("pkg/main.py"));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 3);
        assert_eq!(errors[0].column, 5);
        assert!(errors[0].message.contains("F401"));
    }

    #[test]
    fn parse_staticcheck_output_json_lines() {
        let stdout = r#"{"code":"SA4006","severity":"error","location":{"file":"C:\\repo\\main.go","line":10,"column":5},"message":"value is never used"}"#;
        let errors = parse_staticcheck_output(stdout, "", Path::new(r"C:\repo\main.go"));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 10);
        assert_eq!(errors[0].column, 5);
        assert!(errors[0].message.contains("SA4006"));
    }

    #[test]
    fn parse_go_vet_output_handles_windows_drive_letters() {
        let stderr = r"C:\repo\main.go:10:5: unreachable code
C:\repo\other.go:1:1: other file
";
        let errors = parse_go_vet_output(stderr, Path::new(r"C:\repo\main.go"));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 10);
        assert_eq!(errors[0].column, 5);
        assert_eq!(errors[0].message, "unreachable code");
    }

    #[cfg(unix)]
    #[test]
    fn detect_type_checker_biome_uses_json_reporter() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("biome.json"), "{}\n").unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake = bin_dir.join("biome");
        fs::write(&fake, "#!/bin/sh\necho 1.0.0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let path = dir.path().join("src/app.ts");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };

        let (cmd, args) = detect_type_checker(&path, LangId::TypeScript, &config).unwrap();
        assert!(cmd.ends_with("biome"), "expected biome, got: {cmd}");
        assert_eq!(args[0], "check");
        assert!(args.contains(&"--reporter=json".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn detect_type_checker_ruff_does_not_require_formatter_version() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ruff.toml"), "\n").unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake = bin_dir.join("ruff");
        fs::write(&fake, "#!/bin/sh\necho 'ruff 0.0.1'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let path = dir.path().join("main.py");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };

        assert!(!ruff_format_available(config.project_root.as_deref()));
        let (cmd, args) = detect_type_checker(&path, LangId::Python, &config).unwrap();
        assert!(cmd.ends_with("ruff"), "expected ruff checker, got: {cmd}");
        assert_eq!(args[0], "check");
        assert!(args.contains(&"--output-format=json".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn detect_type_checker_staticcheck_uses_json_reporter() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module test\ngo 1.21\n").unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake = bin_dir.join("staticcheck");
        fs::write(&fake, "#!/bin/sh\necho staticcheck\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let path = dir.path().join("main.go");
        let config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };

        let (cmd, args) = detect_type_checker(&path, LangId::Go, &config).unwrap();
        assert!(
            cmd.ends_with("staticcheck"),
            "expected staticcheck, got: {cmd}"
        );
        assert_eq!(args[0], "-f");
        assert_eq!(args[1], "json");
    }

    #[cfg(unix)]
    #[test]
    fn detect_type_checker_uses_resolved_cargo_and_go_paths() {
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        fs::create_dir_all(&bin_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for name in ["cargo", "go"] {
            let fake = bin_dir.join(name);
            fs::write(&fake, "#!/bin/sh\necho fake\n").unwrap();
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\n",
        )
        .unwrap();
        let rust_config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let (cargo_cmd, _) =
            detect_type_checker(&dir.path().join("src/main.rs"), LangId::Rust, &rust_config)
                .unwrap();
        assert_eq!(cargo_cmd, bin_dir.join("cargo").to_string_lossy());

        fs::remove_file(dir.path().join("Cargo.toml")).unwrap();
        fs::write(dir.path().join("go.mod"), "module test\ngo 1.21\n").unwrap();
        let mut go_config = Config {
            project_root: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        go_config.checker.insert("go".to_string(), "go".to_string());
        let (go_cmd, _) =
            detect_type_checker(&dir.path().join("main.go"), LangId::Go, &go_config).unwrap();
        assert_eq!(go_cmd, bin_dir.join("go").to_string_lossy());
    }
}

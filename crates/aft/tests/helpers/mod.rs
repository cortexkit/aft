#![allow(dead_code)]

#[path = "../../src/test_env.rs"]
mod shared_test_env;

// Shared test helpers for integration tests.
//
// Provides `AftProcess` — a handle to a running aft binary with piped I/O —
// and `fixture_path` for resolving test fixture files.

#[allow(unused_imports)]
pub(crate) use shared_test_env::{
    apply_hermetic_git_env, hermetic_git_env, hermetic_git_env_guard,
};

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Releases a shell fixture when its owning test scope unwinds.
///
/// Several integration fixtures keep a child shell behind a sentinel file.
/// Writing that file during unwinding lets the child exit before its temporary
/// directory is removed.
pub struct ReleaseOnDrop(pub PathBuf);

impl ReleaseOnDrop {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"release");
    }
}
use std::process::{Child, Command, Stdio};
use std::sync::{
    mpsc::{self, Receiver, RecvTimeoutError},
    Arc, Mutex, Once,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "macos")]
const EXECUTABLE_WARM_TIMEOUT: Duration = Duration::from_secs(120);
/// How many trailing stderr lines the exit-timeout panic keeps in memory for
/// the flake-queue forensics message. Mirrors the LSP client's stderr tail
/// (crates/aft/src/lsp/client.rs) so the two read identically.
const STDERR_TAIL_LINES: usize = 40;

static DISABLE_IN_PROCESS_FILE_WATCHER: Once = Once::new();

/// Disable the real OS watcher for any test that calls `handle_configure`
/// in-process. Those tests do not need a live FSEvents stream, and under heavy
/// macOS load `notify` can wedge forever while stopping an FSEvents watcher if
/// `fseventsd` is saturated.
///
/// Safety: the integration-binary helpers call this before they invoke
/// in-process `handle_configure`, so those code paths never install a real
/// watcher. Child-process tests remain safe because `spawn_inner()` always sets
/// `AFT_TEST_DISABLE_FILE_WATCHER` on the `Command`, and
/// `spawn_with_real_watcher_env()` explicitly overrides it back to `"0"` for
/// real-watcher children. We also verified that the `integration` binary has no
/// test asserting real-watcher behavior in-process; those tests live in the
/// `watcher_integration` binary or use spawned children.
#[allow(unused_unsafe)]
pub fn disable_in_process_file_watcher() {
    DISABLE_IN_PROCESS_FILE_WATCHER.call_once(|| unsafe {
        std::env::set_var("AFT_TEST_DISABLE_FILE_WATCHER", "1");
    });
}

/// Pay macOS's first-exec assessment while a test fixture is still in setup.
///
/// Test fixtures frequently create a new executable inode and then invoke it
/// inside a short product timeout. macOS may assess that first execution long
/// after the file is written, so run the fixture once before the timed path.
/// Callers choose arguments that their fixture handles quickly and without
/// changing the assertion state; the helper always supplies closed stdin and
/// discards output and exit status.
///
/// The warm is advisory. On macOS the spawn runs in a worker thread and this
/// function returns after two minutes even if a security assessment is wedged.
/// Other platforms do nothing because they do not have this assessment delay.
pub fn warm_executable(path: &Path, args: &[&str]) {
    #[cfg(target_os = "macos")]
    {
        let path = path.to_path_buf();
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let (done_tx, done_rx) = mpsc::channel();

        let _ = std::thread::Builder::new()
            .name("aft-test-exec-warm".to_string())
            .spawn(move || {
                let result = (|| -> std::io::Result<()> {
                    let mut child = Command::new(path)
                        .args(args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()?;
                    let deadline = Instant::now() + EXECUTABLE_WARM_TIMEOUT;

                    loop {
                        match child.try_wait()? {
                            Some(_) => return Ok(()),
                            None if Instant::now() >= deadline => {
                                let _ = child.kill();
                                return Ok(());
                            }
                            None => std::thread::sleep(Duration::from_millis(10)),
                        }
                    }
                })();
                let _ = done_tx.send(result);
            });

        let _ = done_rx.recv_timeout(EXECUTABLE_WARM_TIMEOUT);
    }

    #[cfg(not(target_os = "macos"))]
    let _ = (path, args);
}

enum StdoutEvent {
    Json(serde_json::Value),
    Eof,
    IoError(String),
    ParseError { line: String, error: String },
}

/// A handle to a running aft process with piped I/O.
///
/// Uses a persistent `BufReader` over stdout so sequential reads
/// don't lose buffered data between calls.
pub struct AftProcess {
    child: Child,
    stdout_rx: Receiver<StdoutEvent>,
    stdout_eof: bool,
    pending_frames: VecDeque<serde_json::Value>,
    diag_enabled: bool,
    spawned_at: Instant,
    stdout_trace_log_path: Option<PathBuf>,
    stdout_trace_log: Option<Arc<Mutex<std::fs::File>>>,
    stdout_capture_thread: Option<JoinHandle<()>>,
    stderr_log_path: Option<PathBuf>,
    stderr_capture_thread: Option<JoinHandle<String>>,
    /// Rolling tail of the child's stderr, kept by the always-on drain thread
    /// so the exit-timeout panic can report what the child was saying right
    /// before it hung. Shared with the AFT_TEST_DIAG capture thread.
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    _cache_dir: tempfile::TempDir,
}

impl AftProcess {
    pub fn cache_dir(&self) -> &Path {
        self._cache_dir.path()
    }

    /// Spawn the aft binary with piped stdin/stdout/stderr.
    /// Sets AFT_CACHE_DIR to a temp path so tests don't pollute the user's cache.
    pub fn spawn() -> Self {
        Self::spawn_inner(&[])
    }

    /// Spawn the aft binary with additional environment variables.
    /// Stderr is captured in memory (for the exit-timeout panic message) but
    /// not echoed to the test process unless `AFT_TEST_DIAG=1` is set.
    pub fn spawn_with_env(envs: &[(&str, &std::ffi::OsStr)]) -> Self {
        Self::spawn_inner(envs)
    }

    /// Spawn with stderr piped so tests can read it via `stderr_output()`.
    ///
    /// Stderr is now always piped (the drain thread keeps a rolling tail for
    /// the exit-timeout forensics message), so this is equivalent to
    /// `spawn()`; it is kept for existing call sites that read stderr back.
    pub fn spawn_with_stderr() -> Self {
        Self::spawn_inner(&[])
    }

    /// Spawn the aft binary with a REAL OS file watcher installed on configure.
    ///
    /// The default `spawn*` constructors disable the watcher
    /// (`AFT_TEST_DISABLE_FILE_WATCHER=1`) so the ~600 integration spawns don't
    /// collectively swamp the macOS `fseventsd` daemon. Only tests that actually
    /// assert watcher-driven invalidation (mutate a file *outside* AFT's tools
    /// after configure, then expect the index/cache to update) need a real
    /// watcher; those use this constructor. It also enables synchronous watcher
    /// startup (`AFT_TEST_SYNC_FILE_WATCHER_START=1`) so a write right after
    /// configure can't race an un-attached watcher.
    pub fn spawn_with_real_watcher() -> Self {
        Self::spawn_with_real_watcher_env(&[])
    }

    /// Like `spawn_with_real_watcher` but with additional environment variables.
    pub fn spawn_with_real_watcher_env(envs: &[(&str, &std::ffi::OsStr)]) -> Self {
        let mut full: Vec<(&str, &std::ffi::OsStr)> = vec![
            ("AFT_TEST_DISABLE_FILE_WATCHER", std::ffi::OsStr::new("0")),
            (
                "AFT_TEST_SYNC_FILE_WATCHER_START",
                std::ffi::OsStr::new("1"),
            ),
        ];
        full.extend_from_slice(envs);
        Self::spawn_inner(&full)
    }

    fn spawn_inner(envs: &[(&str, &std::ffi::OsStr)]) -> Self {
        // Nextest remaps archive binaries into its extraction directory, so its
        // runtime variable must win over Cargo's compile-time binary path.
        let binary = std::env::var_os("AFT_TEST_AFT_BINARY")
            .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_aft"))
            .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_BIN_EXE_aft")));
        let diag_enabled =
            std::env::var_os("AFT_TEST_DIAG").as_deref() == Some(std::ffi::OsStr::new("1"));
        let cache_dir = tempfile::tempdir().expect("create aft test cache dir");
        let mut command = Command::new(binary);
        command
            .envs(hermetic_git_env())
            .env("AFT_CACHE_DIR", cache_dir.path())
            // Callgraph store cold build is pure-async in production (returns
            // `Building`, agent retries). Fixture projects are tiny (build in
            // ~100ms), so default the test harness to a large inline-wait window
            // so the first callgraph op resolves to `Ready` synchronously. A
            // failed build disconnects the channel and returns immediately, so
            // this never hangs. Lifecycle tests override it to "0" to exercise
            // the real Building -> drain -> Ready path.
            .env("AFT_CALLGRAPH_BUILD_WAIT_MS", "30000")
            // Keep the fast pre-15s semantic quiet window in tests: rigs that
            // assert watcher-driven semantic refresh would otherwise wait out
            // the production burst-coalescing window on every batch.
            .env("AFT_SEMANTIC_QUIET_WINDOW_MS", "50")
            // Disable the OS file watcher by default. ~600 integration spawns
            // each installing a recursive FsEventWatcher swamp the single macOS
            // fseventsd daemon, throttling event delivery and flaking the few
            // tests that wait on watcher-driven invalidation. Real-watcher tests
            // opt back in via spawn_with_real_watcher (which overrides this to
            // "0"). Explicit `envs` below can override it.
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            // Legacy integration fixtures live under the OS temp directory. Tests
            // for the production temp-path policy override this dedicated test hook.
            .env("AFT_TEST_ALLOW_TEMP_BACKUPS", "1")
            // Keep the child's PATH exactly what the test constructed. The
            // production login-shell probe + standard-dir enrichment would
            // re-add real tool dirs (e.g. /usr/local/bin on CI runners),
            // breaking tests that simulate missing formatters/checkers by
            // building an impoverished PATH.
            .env("AFT_TEST_RAW_PATH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Stderr is always piped. A drain thread (spawn_stderr_drain_thread)
            // keeps a rolling tail so the exit-timeout panic can report what
            // the child was doing right before it hung — the flake-queue
            // forensics this whole change exists for. Under AFT_TEST_DIAG=1 the
            // same drain also tees to a log file and the test process's stderr;
            // without DIAG the tail is still captured in memory for the panic
            // message, and stderr_output() reads it back from the buffer.
            .stderr(Stdio::piped());

        for (key, value) in envs {
            command.env(key, value);
        }

        #[cfg(windows)]
        let mut child = {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                match command.spawn() {
                    Ok(child) => break child,
                    // Windows Application Control can temporarily reject a freshly built
                    // test binary while its trust check is still in flight. Retry only that
                    // observable policy result; permanent policy failures still surface at
                    // the deadline and all other spawn errors fail immediately.
                    Err(error)
                        if error.raw_os_error() == Some(4551) && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(error) => panic!("failed to spawn aft binary: {error:?}"),
                }
            }
        };
        #[cfg(not(windows))]
        let mut child = command.spawn().expect("failed to spawn aft binary");
        let child_pid = child.id();

        let spawned_at = Instant::now();
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));

        let (stdout_trace_log_path, stdout_trace_log, stderr_log_path, stderr_capture_thread) =
            if diag_enabled {
                let target_tmpdir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
                std::fs::create_dir_all(&target_tmpdir).expect("create CARGO_TARGET_TMPDIR");

                let stdout_trace_log_path =
                    target_tmpdir.join(format!("aft-test-stdout-trace-{child_pid}.log"));
                let stdout_trace_log = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&stdout_trace_log_path)
                    .expect("open aft stdout trace log");
                let stdout_trace_log = Arc::new(Mutex::new(stdout_trace_log));

                let stderr_log_path =
                    target_tmpdir.join(format!("aft-test-stderr-{child_pid}.log"));
                let stderr = child.stderr.take().expect("stderr handle");
                // DIAG mode: tee to the log file + the test process's stderr
                // (existing behavior) AND feed the in-memory tail ring buffer
                // so the exit-timeout panic can quote it without reading a file.
                let stderr_capture_thread = Some(spawn_stderr_capture_thread(
                    stderr,
                    stderr_log_path.clone(),
                    Arc::clone(&stderr_tail),
                ));

                (
                    Some(stdout_trace_log_path),
                    Some(stdout_trace_log),
                    Some(stderr_log_path),
                    stderr_capture_thread,
                )
            } else {
                // Non-DIAG mode: stderr is still piped (so the exit-timeout
                // panic has a tail to quote), but it is NOT echoed to the test
                // process's stderr and NOT written to a log file — preserving
                // the "stderr is suppressed by default" contract that lets
                // ~600 integration spawns run without noisy interleaving. Only
                // the in-memory tail ring buffer is kept.
                let stderr = child.stderr.take().expect("stderr handle");
                let stderr_capture_thread = Some(spawn_stderr_tail_only_thread(
                    stderr,
                    Arc::clone(&stderr_tail),
                ));
                (None, None, None, stderr_capture_thread)
            };

        let stdout = child.stdout.take().expect("stdout handle");
        let (stdout_rx, stdout_capture_thread) =
            spawn_stdout_capture_thread(stdout, stdout_trace_log.clone(), spawned_at);

        AftProcess {
            child,
            stdout_rx,
            stdout_eof: false,
            pending_frames: VecDeque::new(),
            diag_enabled,
            spawned_at,
            stdout_trace_log_path,
            stdout_trace_log,
            stdout_capture_thread: Some(stdout_capture_thread),
            stderr_log_path,
            stderr_capture_thread,
            stderr_tail,
            _cache_dir: cache_dir,
        }
    }

    /// Send a raw line and read back the JSON response.
    pub fn send(&mut self, request: &str) -> serde_json::Value {
        self.send_with_timeout(request, DEFAULT_RESPONSE_TIMEOUT)
    }

    pub fn send_with_timeout(&mut self, request: &str, timeout: Duration) -> serde_json::Value {
        let stdin = self.child.stdin.as_mut().expect("stdin handle");
        writeln!(stdin, "{}", request).expect("write to stdin");
        stdin.flush().expect("flush stdin");

        let request_id = serde_json::from_str::<serde_json::Value>(request)
            .ok()
            .and_then(|value| value["id"].as_str().map(str::to_string));
        loop {
            let value = self.read_json_line_timeout(timeout, "response line");
            if value.get("type").is_some() && value.get("id").is_none() {
                self.pending_frames.push_back(value);
                continue;
            }
            if request_id
                .as_deref()
                .is_none_or(|request_id| value["id"] == request_id)
            {
                return value;
            }
            return value;
        }
    }

    /// Read the next JSON line from stdout without writing a request first.
    pub fn read_next(&mut self) -> serde_json::Value {
        if let Some(value) = self.pending_frames.pop_front() {
            return value;
        }
        self.read_json_line()
    }

    /// Try to read one JSON line from stdout within a short timeout.
    pub fn try_read_next_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Option<serde_json::Value> {
        if let Some(value) = self.pending_frames.pop_front() {
            return Some(value);
        }

        if self.stdout_eof {
            return None;
        }

        match self.stdout_rx.recv_timeout(timeout) {
            Ok(StdoutEvent::Json(value)) => Some(value),
            Ok(StdoutEvent::Eof) => {
                self.stdout_eof = true;
                None
            }
            Ok(StdoutEvent::IoError(error)) => panic!("read from stdout: {error}"),
            Ok(StdoutEvent::ParseError { line, error }) => {
                panic!("parse response JSON: {error}; line: {line}")
            }
            Err(RecvTimeoutError::Timeout) => {
                self.trace_event(&format!(
                    "STDOUT_POLL_TIMEOUT (no data within {}ms)",
                    timeout.as_millis()
                ));
                None
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.stdout_eof = true;
                None
            }
        }
    }

    fn read_json_line(&mut self) -> serde_json::Value {
        self.read_json_line_timeout(DEFAULT_RESPONSE_TIMEOUT, "response line")
    }

    fn read_json_line_timeout(&mut self, timeout: Duration, context: &str) -> serde_json::Value {
        assert!(
            !self.stdout_eof,
            "expected {context} but stdout was already at EOF from aft"
        );
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(StdoutEvent::Json(value)) => value,
            Ok(StdoutEvent::Eof) => {
                self.stdout_eof = true;
                panic!("expected {context} but got EOF from aft");
            }
            Ok(StdoutEvent::IoError(error)) => panic!("read from stdout: {error}"),
            Ok(StdoutEvent::ParseError { line, error }) => {
                panic!("parse response JSON: {error}; line: {line}")
            }
            Err(RecvTimeoutError::Timeout) => {
                let child_status = match self.child.try_wait() {
                    Ok(Some(status)) => format!("exited with status {status}"),
                    Ok(None) => "still running".to_string(),
                    Err(error) => format!("try_wait error: {error}"),
                };
                panic!(
                    "timed out after {}s waiting for {context} from aft stdout (child {child_status})",
                    timeout.as_secs()
                );
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.stdout_eof = true;
                panic!("expected {context} but stdout reader disconnected");
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn queue_pending_frame_for_test(&mut self, frame: serde_json::Value) {
        self.pending_frames.push_back(frame);
    }

    /// Test-only entry point that drives `wait_for_child_exit` with a caller-
    /// supplied short timeout. Used by the exit-timeout forensics unit test to
    /// provoke the panic on a deliberately-still-alive child without widening
    /// any production timeout.
    #[cfg(test)]
    pub(crate) fn wait_for_child_exit_with_timeout(
        &mut self,
        timeout: Duration,
        context: &str,
    ) -> std::process::ExitStatus {
        self.wait_for_child_exit(timeout, context)
    }

    /// Send a configure command with project_root.
    pub fn configure(&mut self, project_root: &std::path::Path) -> serde_json::Value {
        // Build via serde_json so Windows paths (with backslashes) are
        // escaped correctly in the wire format. Hand-formatted JSON would
        // turn `C:\Users\...` into invalid escape sequences.
        let request = serde_json::json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project_root.to_string_lossy(),
        });
        self.send(&request.to_string())
    }

    /// Send a configure command with `format_on_edit: true`.
    ///
    /// The default is now `false` (formatting after an edit can reflow the file
    /// under the agent), so tests that exercise the formatting subsystem
    /// (applied formatting, skip-reason taxonomy, missing-formatter warnings)
    /// must opt in explicitly. The OFF path is covered by its own tests.
    pub fn configure_format_on_edit(
        &mut self,
        project_root: &std::path::Path,
    ) -> serde_json::Value {
        let request = serde_json::json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project_root.to_string_lossy(),
            "config": user_config(serde_json::json!({ "format_on_edit": true })),
        });
        self.send(&request.to_string())
    }

    /// Wait for and consume a `configure_warnings` push frame, returning its
    /// `warnings` array merged into the original configure response.
    ///
    /// Configure now defers the file-walk + missing-binary detection to a
    /// background thread (so it can return in <100 ms even on huge directories).
    /// Tests that previously relied on synchronous warnings should call this
    /// helper after `configure` to merge the async results back in:
    ///
    /// ```rust,ignore
    /// let configure = aft.send(json!({"id":"cfg",...}).to_string().as_str());
    /// let configure = aft.merge_configure_warnings(configure);
    /// // configure["warnings"] now contains the async warnings
    /// ```
    pub fn merge_configure_warnings(
        &mut self,
        mut configure: serde_json::Value,
    ) -> serde_json::Value {
        let frame = self.wait_for_configure_warnings_frame();
        let warnings = frame
            .get("warnings")
            .and_then(|warnings| warnings.as_array())
            .cloned()
            .unwrap_or_default();
        configure["warnings"] = serde_json::Value::Array(warnings);
        configure["warnings_pending"] = serde_json::Value::Bool(false);
        configure
    }

    /// Read frames until a `configure_warnings` push frame arrives, then
    /// return it. Panics if a non-frame response (one with an `id`) arrives
    /// before the frame, or if EOF is hit, or if no frame arrives within 60s.
    fn wait_for_configure_warnings_frame(&mut self) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        let poll_interval = Duration::from_millis(100);
        loop {
            while let Some(value) = self.pending_frames.pop_front() {
                if value.get("type").and_then(|kind| kind.as_str()) == Some("configure_warnings") {
                    return value;
                }
                // Other push frames (progress, bash_completed) are skipped silently.
            }

            let now = Instant::now();
            if now >= deadline {
                if self.diag_enabled {
                    self.panic_configure_warnings_timeout();
                } else {
                    panic!(
                        "timed out waiting for configure_warnings push frame after 60s — \
                         background configure-warnings worker either crashed or progress_sender \
                         was not installed"
                    );
                }
            }
            let remaining = deadline - now;
            let timeout = std::cmp::min(remaining, poll_interval);
            if let Some(value) = self.try_read_next_timeout(timeout) {
                if value.get("type").and_then(|kind| kind.as_str()) == Some("configure_warnings") {
                    return value;
                }
                // Other push frames (progress, bash_completed) are skipped silently.
                continue;
            }
            // No data within poll_interval — loop and re-check deadline.
        }
    }

    /// Send a raw line that should produce no response (e.g. empty line).
    /// Verifies the process is still alive by sending a follow-up ping.
    pub fn send_silent(&mut self, request: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin handle");
        writeln!(stdin, "{}", request).expect("write to stdin");
        stdin.flush().expect("flush stdin");
    }

    /// Send a raw line and collect response lines until `predicate` returns true.
    pub fn send_until<F>(&mut self, request: &str, mut predicate: F) -> Vec<serde_json::Value>
    where
        F: FnMut(&serde_json::Value) -> bool,
    {
        let stdin = self.child.stdin.as_mut().expect("stdin handle");
        writeln!(stdin, "{}", request).expect("write to stdin");
        stdin.flush().expect("flush stdin");

        let mut responses = Vec::new();
        loop {
            let value = self.read_next();
            let done = predicate(&value);
            responses.push(value);
            if done {
                return responses;
            }
        }
    }

    /// Close stdin and wait for the process to exit. Returns the exit status.
    pub fn shutdown(mut self) -> std::process::ExitStatus {
        drop(self.child.stdin.take());
        let status = self.wait_for_child_exit(SHUTDOWN_TIMEOUT, "shutdown after stdin close");
        if let Some(handle) = self.stdout_capture_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_capture_thread.take() {
            // Join the stderr drain thread before returning so parallel test
            // processes cannot leave detached stderr readers interleaving with
            // later tests. This is now always present (stderr is always piped
            // for the exit-timeout forensics tail), not just under
            // AFT_TEST_DIAG=1.
            let _ = handle.join();
        }
        status
    }

    /// Return the PID of the spawned aft process.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Path to the stderr capture log when `AFT_TEST_DIAG=1` is enabled.
    pub fn stderr_log_path(&self) -> Option<&Path> {
        self.stderr_log_path.as_deref()
    }

    /// Path to the stdout trace log when `AFT_TEST_DIAG=1` is enabled.
    pub fn stdout_trace_log_path(&self) -> Option<&Path> {
        self.stdout_trace_log_path.as_deref()
    }

    /// Read stderr contents after process exits.
    pub fn stderr_output(mut self) -> (std::process::ExitStatus, String) {
        drop(self.child.stdin.take());
        let status = self.wait_for_child_exit(SHUTDOWN_TIMEOUT, "stderr_output after stdin close");
        if let Some(handle) = self.stdout_capture_thread.take() {
            let _ = handle.join();
        }
        let mut stderr_content = String::new();
        if let Some(stderr_capture_thread) = self.stderr_capture_thread.take() {
            // The drain thread (DIAG log+tee or tail-only) returns the full
            // captured stderr on join. child.stderr was already taken by the
            // drain thread at spawn time, so there is no handle to read here.
            stderr_content = stderr_capture_thread.join().unwrap_or_default();
        }
        (status, stderr_content)
    }

    fn wait_for_child_exit(
        &mut self,
        timeout: Duration,
        context: &str,
    ) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => {
                    // Flake-queue forensics: before killing the child, snapshot
                    // what it was doing. A Windows-CI-only timeout failure in
                    // `configure_test::configure_does_not_warn_for_file_discovered_non_auto_installable_lsp`
                    // has been sighted multiple times as a bare timeout panic
                    // at this site with zero evidence about the child's state,
                    // and reruns pass — so the next sighting must explain
                    // itself. We capture the stderr tail, pid, and how long we
                    // waited; on Windows we also record the kill result so a
                    // PID-recycle reads differently from a genuine hang. The
                    // control flow is unchanged — this is still a panic — only
                    // the message is richer. No timeout widening (that is the
                    // masking move this repo forbids).
                    let pid = self.child.id();
                    let stderr_tail = self.read_stderr_tail();
                    let waited_secs = timeout.as_secs();
                    // On Windows, record the kill result so a PID-recycle
                    // (process gone between try_wait and kill) reads
                    // differently from a genuine hang. Unix kill results are
                    // not inspected — the stderr tail is the forensics there.
                    #[cfg(windows)]
                    let kill_note = match self.kill_child_for_timeout() {
                        Ok(()) => "kill returned Ok (process was still alive at kill time)".to_string(),
                        Err(error) => format!("kill returned Err: {error} (process may have exited between try_wait and kill — PID recycling?)"),
                    };
                    #[cfg(not(windows))]
                    let kill_note = {
                        let _ = self.kill_child_for_timeout();
                        "kill issued (Unix kill result not inspected; see stderr tail for child state)".to_string()
                    };
                    panic!(
                        "timed out after {waited_secs}s waiting for aft process exit during {context}\n\
                         ↳ child pid: {pid}\n\
                         ↳ {kill_note}\n\
                         ↳ stderr tail (last {STDERR_TAIL_LINES} lines):\n{stderr_tail}"
                    );
                }
                Err(error) => panic!("wait for process exit during {context}: {error}"),
            }
        }
    }

    /// Kill the child and wait for it to be reaped. Returns the kill result
    /// (used by the Windows exit-timeout panic to distinguish a genuine hang
    /// from a PID that was already gone — i.e. PID recycling).
    fn kill_child_for_timeout(&mut self) -> std::io::Result<()> {
        self.child.kill()?;
        let _ = self.child.wait();
        Ok(())
    }

    /// Return the last `STDERR_TAIL_LINES` lines of the child's stderr as a
    /// single string, joined by newlines. Reads from the in-memory ring buffer
    /// fed by the always-on drain thread, so it works even after the child has
    /// been killed and without touching a log file.
    fn read_stderr_tail(&self) -> String {
        let guard = self.stderr_tail.lock();
        match guard {
            Ok(tail) => tail.iter().cloned().collect::<Vec<_>>().join("\n"),
            Err(_) => "<stderr tail mutex poisoned>".to_string(),
        }
    }

    fn trace_event(&mut self, event: &str) {
        if !self.diag_enabled {
            return;
        }
        write_trace_event(&self.stdout_trace_log, self.spawned_at, event);
    }

    fn panic_configure_warnings_timeout(&mut self) -> ! {
        let stderr_log_path = self
            .stderr_log_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<disabled>".to_string());
        let stdout_trace_log_path = self
            .stdout_trace_log_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<disabled>".to_string());

        let child_status = match self.child.try_wait() {
            Ok(Some(status)) => format!("exited with status {status}"),
            Ok(None) => "still running".to_string(),
            Err(error) => format!("try_wait error: {error}"),
        };

        let pending_frames = describe_pending_frames(&self.pending_frames);
        let grace_read_result = self.grace_read_result(Duration::from_secs(5));

        let stderr_capture = read_log_file_tail(self.stderr_log_path.as_deref(), 64 * 1024);
        let stdout_trace = read_log_file_tail(self.stdout_trace_log_path.as_deref(), 64 * 1024);

        eprintln!("===== aft child stderr capture ({stderr_log_path}) =====\n{stderr_capture}");
        eprintln!("===== aft stdout trace ({stdout_trace_log_path}) =====\n{stdout_trace}");

        panic!(
            "timed out waiting for configure_warnings push frame after 60s\n \
             ↳ child status: {child_status}\n \
             ↳ pending_frames queue ({} entries):\n{}\n \
             ↳ stdout trace log: {stdout_trace_log_path}\n \
             ↳ stderr capture log: {stderr_log_path}\n \
             ↳ grace 5s read result: {grace_read_result}\n \
             ↳ full stderr capture:\n{stderr_capture}\n \
             ↳ full stdout trace:\n{stdout_trace}",
            self.pending_frames.len(),
            pending_frames
        );
    }

    fn grace_read_result(&mut self, timeout: Duration) -> String {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.try_read_next_timeout(timeout)
        })) {
            Ok(Some(value)) => format!("frame arrived: {}", value),
            Ok(None) => "timeout".to_string(),
            Err(_) => "io error: try_read_next_timeout panicked; see stdout trace".to_string(),
        }
    }
}

fn spawn_stdout_capture_thread(
    stdout: std::process::ChildStdout,
    trace_log: Option<Arc<Mutex<std::fs::File>>>,
    spawned_at: Instant,
) -> (Receiver<StdoutEvent>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    write_trace_event(&trace_log, spawned_at, "STDOUT_EOF");
                    let _ = tx.send(StdoutEvent::Eof);
                    break;
                }
                Ok(_) => {
                    write_trace_event(
                        &trace_log,
                        spawned_at,
                        &format!("STDOUT_LINE: {}", truncate_for_trace(&line)),
                    );
                    match serde_json::from_str(line.trim()) {
                        Ok(value) => {
                            if tx.send(StdoutEvent::Json(value)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let rendered_line = truncate_for_trace(&line);
                            let _ = tx.send(StdoutEvent::ParseError {
                                line: rendered_line,
                                error: error.to_string(),
                            });
                            break;
                        }
                    }
                }
                Err(error) => {
                    write_trace_event(
                        &trace_log,
                        spawned_at,
                        &format!("STDOUT_READ_ERR: {:?}: {}", error.kind(), error),
                    );
                    let _ = tx.send(StdoutEvent::IoError(format!(
                        "{:?}: {}",
                        error.kind(),
                        error
                    )));
                    break;
                }
            }
        }
    });

    (rx, handle)
}

fn write_trace_event(
    trace_log: &Option<Arc<Mutex<std::fs::File>>>,
    spawned_at: Instant,
    event: &str,
) {
    let Some(log) = trace_log else {
        return;
    };
    let elapsed_ms = spawned_at.elapsed().as_millis();
    let mut log = log.lock().expect("lock aft stdout trace log");
    writeln!(log, "[{}][{}] {}", iso_timestamp_now(), elapsed_ms, event)
        .expect("write aft stdout trace log");
    log.flush().expect("flush aft stdout trace log");
}

fn spawn_stderr_capture_thread(
    stderr: std::process::ChildStderr,
    stderr_log_path: PathBuf,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&stderr_log_path)
            .expect("open aft stderr capture log");
        let mut captured = Vec::new();
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    log.write_all(&buffer)
                        .expect("write aft stderr capture log");
                    log.flush().expect("flush aft stderr capture log");
                    captured.extend_from_slice(&buffer);
                    append_stderr_tail(&stderr_tail, &buffer);
                    eprint!("{}", String::from_utf8_lossy(&buffer));
                }
                Err(error) => {
                    let message = format!("[aft-test stderr capture read error: {error}]\n");
                    log.write_all(message.as_bytes())
                        .expect("write aft stderr capture error");
                    log.flush().expect("flush aft stderr capture error");
                    captured.extend_from_slice(message.as_bytes());
                    append_stderr_tail(&stderr_tail, message.as_bytes());
                    eprint!("{message}");
                    break;
                }
            }
        }
        String::from_utf8_lossy(&captured).into_owned()
    })
}

/// Drain stderr into the in-memory tail ring buffer only — no log file, no
/// `eprint!`. This is the non-DIAG path: stderr stays suppressed (so ~600
/// integration spawns don't interleave noise), but the rolling tail is still
/// available for the exit-timeout panic message. Returns the full captured
/// stderr on join so `stderr_output()` keeps working.
fn spawn_stderr_tail_only_thread(
    stderr: std::process::ChildStderr,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut captured = Vec::new();
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    append_stderr_tail(&stderr_tail, &buffer);
                    captured.extend_from_slice(&buffer);
                }
                Err(error) => {
                    let message = format!("[aft-test stderr capture read error: {error}]\n");
                    append_stderr_tail(&stderr_tail, message.as_bytes());
                    captured.extend_from_slice(message.as_bytes());
                    break;
                }
            }
        }
        String::from_utf8_lossy(&captured).into_owned()
    })
}

/// Append a raw stderr byte chunk (one read_until line) to the rolling tail
/// ring buffer, trimming a trailing newline. Mirrors the LSP client's
/// `append_stderr_tail` (crates/aft/src/lsp/client.rs) so the two read
/// identically.
fn append_stderr_tail(tail: &Arc<Mutex<VecDeque<String>>>, bytes: &[u8]) {
    let Ok(mut guard) = tail.lock() else {
        return;
    };
    let line = String::from_utf8_lossy(bytes);
    let line = line.trim_end_matches(['\r', '\n']);
    if guard.len() == STDERR_TAIL_LINES {
        guard.pop_front();
    }
    guard.push_back(line.to_string());
}

fn truncate_for_trace(line: &str) -> String {
    const LIMIT: usize = 4096;
    let bytes = line.as_bytes();
    let mut rendered = if bytes.len() <= LIMIT {
        line.to_string()
    } else {
        let mut end = LIMIT;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}...[truncated at 4096B, total {}bytes]",
            &line[..end],
            bytes.len()
        )
    };
    if rendered.ends_with('\n') {
        rendered.pop();
        if rendered.ends_with('\r') {
            rendered.pop();
        }
    }
    rendered
}

fn read_log_file_tail(path: Option<&Path>, max_bytes: usize) -> String {
    const MAX: usize = 64 * 1024;
    let max_bytes = max_bytes.min(MAX);
    match path {
        Some(path) => {
            let result = (|| -> std::io::Result<String> {
                let mut file = std::fs::File::open(path)?;
                let len = file.metadata()?.len() as usize;
                if len <= max_bytes {
                    let mut s = String::new();
                    use std::io::Read;
                    file.read_to_string(&mut s)?;
                    return Ok(s);
                }

                use std::io::{Read, Seek, SeekFrom};
                file.seek(SeekFrom::End(-(max_bytes as i64)))?;
                let mut buf = Vec::with_capacity(max_bytes);
                file.read_to_end(&mut buf)?;
                let lossy = String::from_utf8_lossy(&buf).into_owned();
                // Drop the (potentially mid-multibyte) first line so output starts clean.
                let after_first_newline = lossy.find('\n').map(|i| i + 1).unwrap_or(0);
                Ok(format!(
                    "...[truncated to tail {max_bytes}B of total {len}B]\n{}",
                    &lossy[after_first_newline..]
                ))
            })();
            result.unwrap_or_else(|error| format!("<failed to read {}: {error}>", path.display()))
        }
        None => "<disabled>".to_string(),
    }
}

fn describe_pending_frames(pending_frames: &VecDeque<serde_json::Value>) -> String {
    if pending_frames.is_empty() {
        return "    <empty>".to_string();
    }
    pending_frames
        .iter()
        .map(|frame| {
            let frame_type = frame
                .get("type")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let message = frame.get("message").cloned();
            match message {
                Some(message) => format!(
                    "    - {{ type: {}, id: {}, message: {} }}",
                    frame_type, id, message
                ),
                None => format!("    - {{ type: {}, id: {} }}", frame_type, id),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn iso_timestamp_now() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_seconds = duration.as_secs() as i64;
    let millis = duration.subsec_millis();
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

/// Resolve the crate manifest directory.
pub fn cargo_manifest_dir() -> PathBuf {
    normalize_embedded_manifest_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

#[cfg(not(windows))]
fn normalize_embedded_manifest_dir(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
fn normalize_embedded_manifest_dir(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }

    // The Windows VM verification path cross-compiles the test binary on macOS.
    // Cargo embeds a Unix-shaped manifest dir such as `/Users/.../crates/aft`;
    // on Windows that is drive-relative, not absolute, so AFT correctly rejects
    // it as a project root. Map that shape onto the current drive for the VM.
    let raw = path.to_string_lossy();
    let Some(stripped) = raw.strip_prefix('/').or_else(|| raw.strip_prefix('\\')) else {
        return path;
    };
    let Some(drive) = current_windows_drive() else {
        return path;
    };
    PathBuf::from(format!("{drive}:/{stripped}"))
}

#[cfg(windows)]
fn current_windows_drive() -> Option<char> {
    use std::path::{Component, Prefix};

    match std::env::current_dir().ok()?.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                Some((drive as char).to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Resolve a fixture file path relative to the project root.
pub fn fixture_path(name: &str) -> PathBuf {
    cargo_manifest_dir()
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Build a user-tier config entry for configure requests.
pub fn user_config_tier(doc: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "tier": "user",
        "source": "/tmp/aft-test-user-aft.jsonc",
        "doc": doc.to_string(),
    })
}

/// Build a one-entry `config` tier array for configure requests.
pub fn user_config(doc: serde_json::Value) -> serde_json::Value {
    serde_json::json!([user_config_tier(doc)])
}

// ---------------------------------------------------------------------------
// Shared warn-level log capture for integration tests
//
// All integration tests compile into ONE binary, and `log` allows exactly one
// global logger per process. Previously two test modules each installed their
// own logger via `log::set_boxed_logger(...).expect(...)`; whichever ran second
// panicked with `SetLoggerError`, and even without the panic only one module's
// logger could win — the other module's `take_logs()` would read an empty
// buffer because records routed to the winner's storage.
//
// Fix: a SINGLE process-global logger installed once, routing each record to a
// THREAD-LOCAL buffer. Because every `#[test]` runs on its own thread, capture
// is isolated per test and race-free under parallel execution. Tests call
// `init_test_logger()` (installs once, clears this thread's buffer) and
// `take_logs()` (drains this thread's buffer).
// ---------------------------------------------------------------------------

use std::sync::Once as StdOnce;

thread_local! {
    static THREAD_LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

static SHARED_LOGGER_INIT: StdOnce = StdOnce::new();

struct ThreadLocalTestLogger;

impl log::Log for ThreadLocalTestLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let line = format!("{}", record.args());
            THREAD_LOGS.with(|logs| logs.borrow_mut().push(line));
        }
    }

    fn flush(&self) {}
}

/// Install the shared thread-local-capturing logger (once per process) and
/// clear the CURRENT thread's captured logs so each test starts clean.
pub fn init_test_logger() {
    SHARED_LOGGER_INIT.call_once(|| {
        // Ignore an already-installed logger: another harness (or a prior
        // `Once` in legacy code) may have installed one. We only need warn-level
        // capture, and a failed install must never panic the test binary.
        let _ = log::set_boxed_logger(Box::new(ThreadLocalTestLogger));
        log::set_max_level(log::LevelFilter::Warn);
    });
    THREAD_LOGS.with(|logs| logs.borrow_mut().clear());
}

/// Drain and return the warn-level logs captured on the CURRENT thread since
/// the last `init_test_logger()` / `take_logs()`.
pub fn take_logs() -> Vec<String> {
    THREAD_LOGS.with(|logs| std::mem::take(&mut *logs.borrow_mut()))
}

#[cfg(test)]
mod exit_timeout_forensics_tests {
    use super::*;

    /// A deliberately-still-alive `aft` child must produce an exit-timeout
    /// panic whose message carries evidence: the stderr tail (so the next
    /// Windows-CI sighting of the
    /// `configure_test::configure_does_not_warn_for_file_discovered_non_auto_installable_lsp`
    /// timeout flake explains what the child was doing) and the request
    /// context string.
    ///
    /// We spawn `aft`, drive a ping round-trip so the child has emitted its
    /// `[aft] ... started` stderr banner, then call `wait_for_child_exit` with
    /// a 50 ms timeout. The child is still waiting on stdin, so the deadline
    /// elapses and the panic fires. `catch_unwind` lets us inspect the message
    /// without failing the test.
    #[test]
    fn exit_timeout_panic_carries_stderr_tail_and_context() {
        let mut aft = AftProcess::spawn();
        // Drive the child through a request so it has fully initialized its
        // logger and emitted the `[aft] ... started` banner to stderr. A ping
        // round-trip proves the dispatch loop is live and the stderr drain
        // thread has had time to read the startup line into the tail buffer.
        let pong = aft.send(r#"{"id":"forensics-ping","command":"ping"}"#);
        assert_eq!(
            pong["success"], true,
            "ping should succeed before forensics probe: {pong:?}"
        );
        // Give the stderr drain thread a beat to land the startup line.
        std::thread::sleep(Duration::from_millis(50));

        let context = "deliberately-hung-child forensics probe";
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            aft.wait_for_child_exit_with_timeout(Duration::from_millis(50), context)
        }));

        let payload =
            result.expect_err("wait_for_child_exit should have panicked on the still-alive child");
        let message = panic_message_string(&payload);

        assert!(
            message.contains(context),
            "panic message should contain the context string; got: {message}"
        );
        assert!(
            message.contains("stderr tail"),
            "panic message should label the stderr tail section; got: {message}"
        );
        assert!(
            message.contains("[aft]"),
            "panic message should carry the child's stderr tail marker `[aft]`; \
             got: {message}"
        );
        assert!(
            message.contains("child pid"),
            "panic message should report the child pid; got: {message}"
        );
    }

    /// Extract a String from a panic payload (the form `panic!` with a format
    /// string produces).
    fn panic_message_string(payload: &Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = payload.downcast_ref::<&str>() {
            return (*s).to_string();
        }
        if let Some(s) = payload.downcast_ref::<String>() {
            return s.clone();
        }
        "<non-string panic payload>".to_string()
    }
}

/// Canonicalize a path the way the LSP subsystem reports it: canonical form
/// with the Windows verbatim (`\\?\`) prefix stripped. Test expectations that
/// compare against server-reported file paths must use this instead of bare
/// `fs::canonicalize`, whose verbatim output on Windows never matches the
/// normalized paths the product emits.
pub fn canonicalize_like_product(path: &std::path::Path) -> std::path::PathBuf {
    let canonical = std::fs::canonicalize(path).expect("canonicalize test path");
    let display = canonical.display().to_string();
    #[cfg(windows)]
    {
        if let Some(stripped) = display.strip_prefix(r"\\?\UNC\") {
            return std::path::PathBuf::from(format!(r"\\{stripped}"));
        }
        if let Some(stripped) = display.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(stripped);
        }
    }
    std::path::PathBuf::from(display)
}

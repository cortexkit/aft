//! Published conformance cases driven through the real aft --subc process.
use async_trait::async_trait;
use cortexkit_role_harness::{
    Harness, HarnessError, KillPoint, KillReport, PointDeclaration, RouteStamp, Trigger,
};
use cortexkit_role_tool_provider_conformance::{
    CallSpec, Capability, Exchange, ObservedFrame, RouteFailure, ScopedPrincipals,
    ToolProviderSubject, ToolRoute,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU16, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_server,
    connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION},
    read_frame, write_frame,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, Mutex as AsyncMutex},
};

#[path = "helpers/aft_binary.rs"]
mod aft_binary;

const LEGACY_FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/tool_provider_legacy_replies.json"
);

fn legacy_fixtures() -> Value {
    serde_json::from_slice(&std::fs::read(LEGACY_FIXTURES).unwrap()).unwrap()
}

/// PowerShell is optional; every other row remains mandatory for its preset.
/// Selection follows the served catalog, not the test runner's PATH.
fn legacy_rows_for_catalog<'a>(
    fixtures: &'a Value,
    preset: &str,
    tools: &BTreeSet<String>,
) -> Vec<&'a Value> {
    let rows = fixtures["presets"][preset].as_array().unwrap();
    let powershell = tools.contains("powershell");
    assert!(
        !powershell || rows.iter().any(|row| row["tool"] == "powershell"),
        "live {preset} catalog advertises powershell, but no pre-flip powershell baseline is available"
    );
    rows.iter()
        .filter(|row| row["tool"] != "powershell" || powershell)
        .map(|row| {
            // Windows reports a directory write as ERROR_ACCESS_DENIED rather
            // than Unix EISDIR. Pin that native error instead of normalizing it.
            #[cfg(windows)]
            if row["tool"] == "write" {
                return fixtures["platform_overrides"]["windows"]["presets"][preset]
                    .as_array()
                    .and_then(|rows| {
                        rows.iter().find(|native| {
                            native["tool"] == row["tool"] && native["arguments"] == row["arguments"]
                        })
                    })
                    .unwrap_or_else(|| {
                        panic!("no generated Windows legacy baseline for {preset}/write")
                    });
            }
            row
        })
        .collect()
}

fn legacy_row_for_probe<'a>(
    rows: &[&'a Value],
    preset: &str,
    tool: &str,
    arguments: &Value,
) -> &'a Value {
    rows.iter().copied().find(|row| row["tool"] == tool && &row["arguments"] == arguments)
        .unwrap_or_else(|| panic!("live {preset} catalog advertises {tool}, but no pre-flip legacy baseline matches arguments {arguments}"))
}

#[cfg(unix)]
fn catalog_pwsh_path(root: &Path) -> PathBuf {
    root.join("catalog-only-pwsh").join("pwsh")
}

#[cfg(unix)]
fn catalog_pwsh_marker(root: &Path) -> PathBuf {
    let executable = catalog_pwsh_path(root);
    executable.with_file_name(format!(
        "{}.invoked",
        executable.file_name().unwrap().to_string_lossy()
    ))
}

/// An executable-presence fixture only: the schema-valid probe is refused
/// before execution. The marker makes any accidental invocation observable.
#[cfg(unix)]
fn advertise_catalog_powershell(command: &mut Command, root: &Path) -> Result<(), HarnessError> {
    use std::os::unix::fs::PermissionsExt;
    let executable = catalog_pwsh_path(root);
    let directory = executable.parent().unwrap();
    std::fs::create_dir_all(directory).map_err(harness_error)?;
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf invoked > \"$0.invoked\"\nexit 99\n",
    )
    .map_err(harness_error)?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .map_err(harness_error)?;
    eprintln!(
        "PowerShell catalog fixture branch: Unix script {}",
        executable.display()
    );
    let paths = std::iter::once(directory.to_path_buf())
        .chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ))
        .collect::<Vec<_>>();
    let path = std::env::join_paths(paths).map_err(harness_error)?;
    let resolved = which::which_in("pwsh", Some(&path), root).map_err(harness_error)?;
    assert_eq!(
        resolved, executable,
        "PowerShell lookup must select the fixture, not a host executable"
    );
    command.env("PATH", path);
    Ok(())
}

#[cfg(windows)]
fn advertise_catalog_powershell(_command: &mut Command, root: &Path) -> Result<(), HarnessError> {
    let executable = which::which("pwsh.exe").map_err(|error| {
        HarnessError::new(format!(
            "real pwsh.exe is required for this Windows catalog case: {error}"
        ))
    })?;
    // Other conformance cases execute real shell commands on Windows, so a
    // presence-only executable cannot stand in for the installed PowerShell.
    assert!(
        !root.join("catalog-only-pwsh").exists(),
        "Windows uses real pwsh.exe; no script fixture or invocation marker may apply"
    );
    eprintln!(
        "PowerShell catalog branch: Windows real pwsh.exe {}; no fixture invocation marker applies",
        executable.display()
    );
    Ok(())
}

fn powershell_catalog_case_available() -> bool {
    #[cfg(windows)]
    if let Err(error) = which::which("pwsh.exe") {
        assert_ne!(
            std::env::var("AFT_REQUIRE_PWSH").as_deref(),
            Ok("1"),
            "AFT_REQUIRE_PWSH=1 requires real pwsh.exe for the Windows catalog case: {error}"
        );
        eprintln!("SKIP Windows PowerShell catalog case: real pwsh.exe is absent ({error}); set AFT_REQUIRE_PWSH=1 to require it");
        return false;
    }
    true
}

#[cfg(unix)]
#[test]
fn catalog_powershell_fixture_is_discovered_and_marks_execution() {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new(aft_binary::aft_binary());
    advertise_catalog_powershell(&mut command, root.path()).unwrap();
    let executable = catalog_pwsh_path(root.path());
    let marker = catalog_pwsh_marker(root.path());
    assert!(!marker.exists());
    let status = Command::new(executable).status().unwrap();
    assert_eq!(status.code(), Some(99));
    assert_eq!(std::fs::read(marker).unwrap(), b"invoked");
}

#[cfg(windows)]
#[test]
fn catalog_powershell_uses_real_executable_without_marker() {
    if !powershell_catalog_case_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    advertise_catalog_powershell(&mut Command::new(aft_binary::aft_binary()), root.path()).unwrap();
    let executable = which::which("pwsh.exe").unwrap();
    let output = Command::new(executable)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Write-Output AFT_REAL_PWSH",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "installed pwsh.exe must execute the real-shell check"
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "AFT_REAL_PWSH"
    );
    assert!(
        !root.path().join("catalog-only-pwsh").exists(),
        "no fixture invocation marker applies to the real Windows executable"
    );
}

/// Only the marked project path is platform-dependent. Do not rewrite other
/// backslashes, URLs, reply text, or operating-system error messages.
fn expand_legacy_project_paths(value: Value, project: &Path) -> Value {
    fn text(input: &str, project: &str, separator: char) -> String {
        let path = regex::Regex::new(r"\{PROJECT\}(?:[/\\][A-Za-z0-9_.\\/-]+)?").unwrap();
        path.replace_all(input, |capture: &regex::Captures<'_>| {
            let suffix = capture[0].strip_prefix("{PROJECT}").unwrap();
            format!(
                "{project}{}",
                suffix.replace(['/', '\\'], &separator.to_string())
            )
        })
        .into_owned()
    }
    match value {
        Value::String(input) => Value::String(text(
            &input,
            &project.to_string_lossy(),
            std::path::MAIN_SEPARATOR,
        )),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| expand_legacy_project_paths(item, project))
                .collect(),
        ),
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, item)| (key, expand_legacy_project_paths(item, project)))
                .collect(),
        ),
        value => value,
    }
}

#[test]
fn legacy_project_expansion_changes_only_marked_paths() {
    let project = Path::new("fixture").join("project");
    let input = json!({"text":"read: {PROJECT}/.cortexkit/no-such-file.txt; https://example.com/a/b; literal \\n", "error":"Is a directory (os error 21)"});
    let expected = json!({"text":format!("read: {}; https://example.com/a/b; literal \\n", project.join(".cortexkit").join("no-such-file.txt").display()), "error":"Is a directory (os error 21)"});
    assert_eq!(expand_legacy_project_paths(input, &project), expected);
}

fn legacy_probe_key(tool: &str, arguments: &Value) -> String {
    format!("{tool}:{}", serde_json::to_string(arguments).unwrap())
}

/// Report the differing JSON pointer and bounded expected and actual values;
/// keep the complete-reply comparison unchanged.
fn legacy_mismatch_diagnostic(expected: &Value, actual: &Value) -> Option<String> {
    fn difference(
        expected: Option<&Value>,
        actual: Option<&Value>,
        pointer: String,
    ) -> Option<String> {
        if expected == actual {
            return None;
        }
        match (expected, actual) {
            (Some(Value::Object(left)), Some(Value::Object(right))) => {
                let keys: BTreeSet<_> = left.keys().chain(right.keys()).collect();
                keys.into_iter().find_map(|key| {
                    difference(
                        left.get(key),
                        right.get(key),
                        format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")),
                    )
                })
            }
            (Some(Value::Array(left)), Some(Value::Array(right))) => {
                (0..left.len().max(right.len())).find_map(|index| {
                    difference(
                        left.get(index),
                        right.get(index),
                        format!("{pointer}/{index}"),
                    )
                })
            }
            _ => Some(pointer),
        }
    }
    fn bounded(value: Option<&Value>) -> String {
        let text = match value {
            Some(Value::String(text)) => text.clone(),
            Some(value) => value.to_string(),
            None => "<missing>".to_owned(),
        };
        match text.char_indices().nth(400) {
            Some((end, _)) => format!("{}…", &text[..end]),
            None => text,
        }
    }
    let pointer = difference(Some(expected), Some(actual), String::new())?;
    Some(format!(
        "JSON pointer {pointer}: expected={:?}; actual={:?}",
        bounded(expected.pointer(&pointer)),
        bounded(actual.pointer(&pointer))
    ))
}

#[test]
fn legacy_mismatch_diagnostic_names_pointer_and_bounds_both_values() {
    let expected = json!({"content":[{"text":"expected"}]});
    let actual = json!({"content":[{"text":"actual"}]});
    assert_eq!(
        legacy_mismatch_diagnostic(&expected, &actual).unwrap(),
        "JSON pointer /content/0/text: expected=\"expected\"; actual=\"actual\""
    );
    let long = json!({"text":"λ".repeat(401)});
    let other = json!({"text":"β".repeat(401)});
    let diagnostic = legacy_mismatch_diagnostic(&long, &other).unwrap();
    assert!(diagnostic.contains(&format!("{}…", "λ".repeat(400))));
    assert!(diagnostic.contains(&format!("{}…", "β".repeat(400))));
    assert!(!diagnostic.contains(&"λ".repeat(401)));
    assert!(legacy_mismatch_diagnostic(&expected, &expected).is_none());
}

/// Schema-valid calls that either read state or fail before mutating it. Apart
/// from status's pinned snapshot, all responses come from real production paths,
/// including argument validation before shell execution.
fn result_probes() -> Vec<CallSpec> {
    vec![
        ("status".into(), json!({})),
        (
            "bash".into(),
            json!({"command":"exit 7", "wait":true, "pty":true}),
        ),
        (
            "powershell".into(),
            json!({"command":"exit 7", "wait":true, "pty":true}),
        ),
        ("read".into(), json!({"path":".cortexkit/no-such-file.txt"})),
        (
            "write".into(),
            json!({"path":".cortexkit", "content":"unused"}),
        ),
        ("edit".into(), json!({"path":".cortexkit"})),
        ("apply_patch".into(), json!({"patchText":"not a patch"})),
        ("grep".into(), json!({"pattern":"["})),
        (
            "glob".into(),
            json!({"pattern":"*", "path":".tool-result-missing-directory"}),
        ),
        ("search".into(), json!({"pattern":"["})),
        ("outline".into(), json!({"target":".cortexkit/no-such.rs"})),
        (
            "zoom".into(),
            json!({"batch":[{"target":".cortexkit/no-such.rs", "symbols":"missing"}]}),
        ),
        (
            "inspect".into(),
            json!({"sections":"__invalid_probe_section__"}),
        ),
        (
            "callgraph".into(),
            json!({"op":"callers", "path":".cortexkit/aft.jsonc", "symbol":"missing"}),
        ),
        ("conflicts".into(), json!({"path":".cortexkit/aft.jsonc"})),
        (
            "ast_search".into(),
            json!({"pattern":"catch", "lang":"javascript"}),
        ),
        (
            "ast_replace".into(),
            json!({"pattern":"console.log($MSG)", "rewrite":"logger.info($$$)", "lang":"javascript", "dryRun":true}),
        ),
        ("delete".into(), json!({"files":[".cortexkit"]})),
        (
            "move".into(),
            json!({"path":".cortexkit", "destination":"tool-result-moved"}),
        ),
        (
            "import".into(),
            json!({"op":"organize", "path":".cortexkit/aft.jsonc"}),
        ),
        (
            "safety".into(),
            json!({"op":"restore", "name":"tool-result-no-such-checkpoint"}),
        ),
        (
            "bash_status".into(),
            json!({"taskId":"tool-result-no-such-task"}),
        ),
        (
            "bash_kill".into(),
            json!({"taskId":"tool-result-no-such-task"}),
        ),
        (
            "bash_write".into(),
            json!({"taskId":"tool-result-no-such-task", "input":"unused"}),
        ),
        (
            "bash_watch".into(),
            json!({"taskId":"tool-result-no-such-task", "pattern":{"regex":"["}, "timeoutMs":1}),
        ),
    ]
}

/// The provider under test, fetching its catalog with `preset` (`None` sends
/// no preset, which AFT serves as `head`).
struct Subject {
    preset: Option<&'static str>,
    /// Launches the provider under test with the main-thread stack Windows
    /// gives it, instead of this host's.
    one_mib_main_stack: bool,
    project: Mutex<Option<PathBuf>>,
    served_tools: Mutex<Option<BTreeSet<String>>>,
    capture_live_status: bool,
    catalog_powershell: bool,
    legacy_diagnostics: Arc<Mutex<HashMap<String, Value>>>,
    /// Stands in for the consumer's elicitation provider: the permission
    /// questions AFT files for the runner's held calls arrive here.
    stand_in: Arc<StandIn>,
}
impl Subject {
    fn new(preset: Option<&'static str>) -> Self {
        Self {
            preset,
            one_mib_main_stack: false,
            project: Mutex::new(None),
            served_tools: Mutex::new(None),
            capture_live_status: false,
            catalog_powershell: false,
            legacy_diagnostics: Arc::default(),
            stand_in: Arc::default(),
        }
    }
    fn head() -> Self {
        Self::new(None)
    }
    fn with_one_mib_main_stack(mut self) -> Self {
        self.one_mib_main_stack = true;
        self
    }
    fn with_catalog_powershell(mut self) -> Self {
        self.catalog_powershell = true;
        self
    }
    fn assert_catalog_powershell_fixture_policy(&self) {
        if self.catalog_powershell {
            let project = self.project.lock().unwrap();
            let root = project.as_ref().unwrap().parent().unwrap();
            #[cfg(unix)]
            assert!(
                !catalog_pwsh_marker(root).exists(),
                "catalog-only pwsh fixture was executed"
            );
            #[cfg(windows)]
            assert!(
                !root.join("catalog-only-pwsh").exists(),
                "real Windows pwsh.exe has no test-owned invocation marker"
            );
        }
    }
    /// The reader preset serves no shell tool, so it has no call AFT can
    /// hold, cancel or settle later.
    fn serves_shell(&self) -> bool {
        self.preset != Some("reader")
    }
}

/// The stand-in elicitation provider. AFT's question carries no call key
/// (only the command), so a question is matched to the runner's key by the
/// marker path the held call's command writes: the runner derives that path
/// from the key (`marker_path(work_dir, key)` in the conformance crate).
#[derive(Default)]
struct StandIn {
    /// The runner's marker directory, learned from the first `held_call`.
    /// Questions for commands outside it go to the ordinary frame queue.
    markers: Mutex<Option<PathBuf>>,
    asks: Mutex<Vec<FiledAsk>>,
}
struct FiledAsk {
    command: String,
    wire: Arc<Wire>,
    channel: u16,
    epoch: u32,
    corr: u64,
    answered: bool,
}
impl StandIn {
    /// The command of a question this stand-in answers, if `frame` is one.
    fn claims(&self, frame: &Frame) -> Option<String> {
        let markers = self.markers.lock().unwrap().clone()?;
        let body: Value = serde_json::from_slice(&frame.body).ok()?;
        let command = body["params"]["_meta"]["aft"]["command"].as_str()?;
        command
            .contains(&markers.display().to_string())
            .then(|| command.to_owned())
    }
    fn marker(&self, key: &str) -> Result<String, HarnessError> {
        let markers = self
            .markers
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| HarnessError::new("no held call was made yet"))?;
        Ok(markers
            .join(format!("{}.marker", key.replace('/', "_")))
            .display()
            .to_string())
    }
    /// Waits until the question for `key` has arrived, then runs `take` on it.
    async fn wait_for<T>(
        &self,
        key: &str,
        within: Duration,
        mut take: impl FnMut(&mut FiledAsk) -> Option<T>,
    ) -> Result<T, HarnessError> {
        let marker = self.marker(key)?;
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Some(found) = self
                .asks
                .lock()
                .unwrap()
                .iter_mut()
                .filter(|ask| ask.command.contains(&marker))
                .find_map(&mut take)
            {
                return Ok(found);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(HarnessError::new(format!(
                    "no permission question for {key} arrived within {within:?}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct Process {
    child: Mutex<Child>,
    wire: Arc<Wire>,
    reader: tokio::task::JoinHandle<()>,
    root: PathBuf,
}
/// One module connection. A reader task demultiplexes the module's frames:
/// a reply goes to the request waiting on its `(channel, corr)`, a question
/// for a runner's held call goes to the stand-in, and everything else to one
/// ordered queue that the exclusive [`WireGuard`] reads.
struct Wire {
    writer: AsyncMutex<tokio::net::tcp::OwnedWriteHalf>,
    unclaimed: AsyncMutex<mpsc::UnboundedReceiver<Result<Frame, String>>>,
    waiters: Mutex<HashMap<(u16, u64), mpsc::UnboundedSender<Frame>>>,
    next_corr: AtomicU64,
    next_channel: AtomicU16,
    last_exchange_outcome: Mutex<Option<String>>,
}
impl Wire {
    fn corr(&self) -> u64 {
        self.next_corr.fetch_add(1, Ordering::SeqCst)
    }
    async fn send(&self, frame: &Frame) -> Result<(), HarnessError> {
        write_frame(&mut *self.writer.lock().await, frame)
            .await
            .map_err(harness_error)
    }
    /// Routes the module's frames for `(channel, corr)` to the returned
    /// receiver until the guard drops.
    fn expect(self: &Arc<Self>, channel: u16, corr: u64) -> Waiter {
        let (tx, rx) = mpsc::unbounded_channel();
        self.waiters.lock().unwrap().insert((channel, corr), tx);
        Waiter {
            wire: self.clone(),
            key: (channel, corr),
            rx,
        }
    }
    /// Exclusive, ordered access to the frames no waiter or stand-in claims.
    async fn lock(&self) -> WireGuard<'_> {
        WireGuard {
            wire: self,
            unclaimed: self.unclaimed.lock().await,
        }
    }
}
struct Waiter {
    wire: Arc<Wire>,
    key: (u16, u64),
    rx: mpsc::UnboundedReceiver<Frame>,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        self.wire.waiters.lock().unwrap().remove(&self.key);
    }
}
struct WireGuard<'a> {
    wire: &'a Wire,
    unclaimed: tokio::sync::MutexGuard<'a, mpsc::UnboundedReceiver<Result<Frame, String>>>,
}
impl WireGuard<'_> {
    fn corr(&self) -> u64 {
        self.wire.corr()
    }
    async fn send(&self, frame: &Frame) -> Result<(), HarnessError> {
        self.wire.send(frame).await
    }
    async fn next(&mut self) -> Result<Frame, HarnessError> {
        tokio::time::timeout(Duration::from_secs(30), self.unclaimed.recv())
            .await
            .map_err(harness_error)?
            .ok_or_else(|| HarnessError::new("module EOF"))?
            .map_err(HarnessError::new)
    }
}
#[derive(Clone)]
struct Route {
    wire: Arc<Wire>,
    channel: u16,
    scoped_preset: Option<&'static str>,
    legacy_diagnostics: Option<Arc<Mutex<HashMap<String, Value>>>>,
}
impl Drop for Process {
    fn drop(&mut self) {
        self.reader.abort();
        let child = self
            .child
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = child.kill();
        let _ = child.wait();
        if std::thread::panicking() || std::env::var_os("AFT_TEST_TRACE_DAEMON_STDERR").is_some() {
            eprintln!(
                "conformance daemon stderr for {}:\n{}",
                self.root.display(),
                std::fs::read_to_string(self.root.join("daemon.stderr"))
                    .unwrap_or_else(|error| error.to_string())
            );
        }
    }
}
fn flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}
fn harness_error(e: impl std::fmt::Display) -> HarnessError {
    HarnessError::new(e.to_string())
}
async fn next_frame(stream: &mut TcpStream) -> Result<Frame, HarnessError> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), read_frame(stream))
            .await
            .map_err(harness_error)?
            .map_err(harness_error)?
            .ok_or_else(|| HarnessError::new("module EOF"))?;
        if frame.header.ty == FrameType::Ping {
            let pong = Frame::build(FrameType::Pong, flags(), 0, 0, frame.header.corr, vec![])
                .map_err(harness_error)?;
            write_frame(stream, &pong).await.map_err(harness_error)?;
            continue;
        }
        if frame.header.ty == FrameType::Push {
            continue;
        }
        return Ok(frame);
    }
}
/// Reads every frame the module sends after the handshake; see [`Wire`].
fn spawn_reader(
    mut read: tokio::net::tcp::OwnedReadHalf,
    wire: Arc<Wire>,
    unclaimed: mpsc::UnboundedSender<Result<Frame, String>>,
    stand_in: Arc<StandIn>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let frame = match read_frame(&mut read).await {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    let _ = unclaimed.send(Err("module EOF".into()));
                    break;
                }
                Err(error) => {
                    let _ = unclaimed.send(Err(error.to_string()));
                    break;
                }
            };
            match frame.header.ty {
                FrameType::Ping => {
                    if let Ok(pong) =
                        Frame::build(FrameType::Pong, flags(), 0, 0, frame.header.corr, vec![])
                    {
                        let _ = wire.send(&pong).await;
                    }
                    continue;
                }
                FrameType::Push => continue,
                FrameType::Request if frame.header.channel != 0 => {
                    if let Some(command) = stand_in.claims(&frame) {
                        stand_in.asks.lock().unwrap().push(FiledAsk {
                            command,
                            wire: wire.clone(),
                            channel: frame.header.channel,
                            epoch: frame.header.epoch,
                            corr: frame.header.corr,
                            answered: false,
                        });
                        continue;
                    }
                }
                _ => {
                    let waiter = wire
                        .waiters
                        .lock()
                        .unwrap()
                        .get(&(frame.header.channel, frame.header.corr))
                        .cloned();
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(frame);
                        continue;
                    }
                }
            }
            let _ = unclaimed.send(Ok(frame));
        }
        // A dead module answers nothing more: end every waiting request.
        wire.waiters.lock().unwrap().clear();
    })
}
fn prepare_provider_project(root: &Path) -> Result<(), HarnessError> {
    // Construct the config location from the same components on every platform.
    let directory = root.join("project").join(".cortexkit");
    std::fs::create_dir_all(&directory).map_err(|error| {
        HarnessError::new(format!(
            "creating provider config directory {}: {error}",
            directory.display()
        ))
    })?;
    let config = directory.join("aft.jsonc");
    std::fs::write(&config, serde_json::to_vec(&json!({"disabled_tools": ["aft_outline"], "indexes": {"callgraph": false, "trigram": false, "semantic": false}})).unwrap()).map_err(|error| HarnessError::new(format!("writing provider config {}: {error}", config.display())))?;
    Ok(())
}

/// The main-thread stack the provider under test may have in this test, the
/// size Windows reserves by default. `None` means it cannot be set here: macOS
/// refuses to lower a forked child's stack limit (`setrlimit` fails with
/// EINVAL), so the case runs on Linux, where the limit is lowered, and on
/// Windows, where the binary's own reservation is checked.
fn windows_sized_main_stack() -> Option<u64> {
    if cfg!(target_os = "macos") {
        None
    } else {
        Some(1024 * 1024)
    }
}

fn provider_command(root: &Path, connection_path: &Path) -> Result<Command, HarnessError> {
    std::fs::create_dir_all(root).map_err(|error| {
        HarnessError::new(format!("creating provider cwd {}: {error}", root.display()))
    })?;
    let mut command = Command::new(aft_binary::aft_binary());
    // CreateProcess uses the parent's cwd when none is supplied. Always launch
    // inside this fixture, independently of the test runner's working directory.
    command.current_dir(root);
    for (variable, component) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        let directory = root.join(component);
        std::fs::create_dir_all(&directory).map_err(|error| {
            HarnessError::new(format!(
                "creating {variable} {}: {error}",
                directory.display()
            ))
        })?;
        command.env(variable, directory);
    }
    command
        .arg("--subc")
        .arg(connection_path)
        .env_remove("SUBC_MODULE_ID")
        .env_remove("SUBC_LAUNCH_NONCE")
        .env_remove("AFT_STORAGE_DIR")
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env(
            "AFT_TEST_DATABASE_OPEN_GATE",
            root.join("database-open-gate"),
        )
        .env(
            "AFT_TEST_CALL_LEDGER_CONTROL",
            root.join("ledger-control.json"),
        )
        .env(
            "AFT_TEST_LEDGER_ADMISSION_GATE",
            root.join("admission-gate"),
        )
        .env(
            "AFT_TEST_RECOVERED_STDOUT_READ_FAILURE_FILE",
            root.join("recovered-stdout-read-failure"),
        )
        .env(
            "AFT_TEST_BASH_AFTER_CREATE_GATE",
            root.join("after-create-gate"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("daemon.stderr"))
                .map_err(harness_error)?,
        ));
    Ok(command)
}

#[test]
fn provider_command_uses_existing_fixture_cwd_and_xdg_directories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("not-yet-created");
    let command = provider_command(&root, &root.join("connection.json")).unwrap();
    assert_eq!(command.get_current_dir(), Some(root.as_path()));
    if let Some(binary) = std::env::var_os("NEXTEST_BIN_EXE_aft") {
        if std::env::var_os("AFT_TEST_AFT_BINARY").is_none() {
            assert_eq!(
                command.get_program(),
                binary,
                "use nextest's relocated executable"
            );
        }
    }
    assert!(root.is_dir());
    let directories: Vec<_> = command
        .get_envs()
        .filter(|(name, _)| name.to_string_lossy().starts_with("XDG_"))
        .collect();
    assert_eq!(directories.len(), 4);
    for (_, value) in directories {
        let path = Path::new(value.unwrap());
        assert!(path.is_dir(), "{} must exist before spawn", path.display());
        assert_eq!(path.parent(), Some(root.as_path()));
    }
    // --version exits without attaching, so this directly exercises OS process
    // creation with the fixture cwd and env rather than requiring a fake daemon.
    let mut probe = provider_command(&root, &root.join("connection.json")).unwrap();
    probe.args(["--version"]);
    assert!(probe.status().unwrap().success());
}

#[test]
fn provider_project_setup_accepts_absent_and_canonical_roots() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent").join("main");
    prepare_provider_project(&absent).unwrap();
    let canonical = absent.canonicalize().unwrap();
    prepare_provider_project(&canonical).unwrap();
    let config: Value = serde_json::from_slice(
        &std::fs::read(
            canonical
                .join("project")
                .join(".cortexkit")
                .join("aft.jsonc"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(config["disabled_tools"], json!(["aft_outline"]));
    assert_eq!(config["indexes"]["callgraph"], false);
    assert!(canonical.join("project").join(".cortexkit").is_dir());
}

#[async_trait]
impl Harness for Subject {
    type Handle = Process;
    type Route = Route;
    /// With `test-timing-hooks` the binary blocks at the `Prepared` and
    /// `Authorized` ledger commits when armed (see `kill_at`), and the
    /// harness then kills the process: a fault hook followed by a real kill.
    fn declared_points(&self) -> Vec<PointDeclaration> {
        #[cfg(feature = "test-timing-hooks")]
        {
            use cortexkit_role_tool_provider::points;
            [points::PREPARED, points::AUTHORIZED]
                .into_iter()
                .map(|point| PointDeclaration {
                    point: KillPoint::new(point),
                    mechanisms: vec![
                        cortexkit_role_harness::KillMechanism::FaultHookThenProcessKill,
                    ],
                    note: "the call ledger is SQLite, so truncation is not meaningful: the \
                           binary blocks right after committing the point and is then killed"
                        .into(),
                })
                .collect()
        }
        #[cfg(not(feature = "test-timing-hooks"))]
        {
            vec![]
        }
    }
    async fn spawn(&self, root: &Path) -> Result<Process, HarnessError> {
        prepare_provider_project(root)?;
        // Replies echo the spelling sent in BindIdentity. Canonical aliases name
        // the same files, but substituting them would change the expected bytes.
        *self.project.lock().unwrap() = Some(root.join("project"));
        *self.served_tools.lock().unwrap() = None;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(harness_error)?;
        let connection = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().map_err(harness_error)?.port(),
            }],
            key: vec![0x42; subc_transport::KEY_LEN],
            daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
            pid: std::process::id(),
            daemon_ver: "tool-provider-conformance".into(),
        };
        let connection_path = root.join("connection.json");
        connection_file::write_atomic(&connection_path, &connection).map_err(|error| {
            HarnessError::new(format!(
                "publishing connection file {}: {error}",
                connection_path.display()
            ))
        })?;
        let mut command = provider_command(root, &connection_path)?;
        if let Some(bytes) = windows_sized_main_stack().filter(|_| self.one_mib_main_stack) {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                // The limit applies to this child only: the test runner and
                // every worker thread keep the stack they had.
                unsafe {
                    command.pre_exec(move || {
                        let mut current: libc::rlimit = std::mem::zeroed();
                        if libc::getrlimit(libc::RLIMIT_STACK, &mut current) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        current.rlim_cur = bytes.min(current.rlim_max);
                        if libc::setrlimit(libc::RLIMIT_STACK, &current) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            #[cfg(windows)]
            {
                // A Windows main-thread stack is reserved by the executable's
                // PE header before any code runs, so it can be checked here but
                // not changed: this case runs only if the binary under test
                // already reserves no more than the stack Windows gives a
                // process by default.
                let binary = std::fs::read(aft_binary::aft_binary()).map_err(harness_error)?;
                let pe = u32::from_le_bytes(binary[0x3c..0x40].try_into().unwrap()) as usize;
                assert_eq!(&binary[pe..pe + 4], b"PE\0\0");
                // PE32+ optional header: its magic at its start, and the size
                // of the initial stack reservation at offset 72.
                let optional = pe + 24;
                assert_eq!(&binary[optional..optional + 2], &[0x0b, 0x02]);
                let reserve =
                    u64::from_le_bytes(binary[optional + 72..optional + 80].try_into().unwrap());
                assert!(
                    reserve <= bytes,
                    "this provider reserves {reserve} bytes of main-thread stack, more \
                     than the {bytes} Windows gives by default, so it cannot run this \
                     case here"
                );
            }
        }
        if self.catalog_powershell {
            advertise_catalog_powershell(&mut command, root)?;
        }
        command.env(
            "AFT_TEST_CONFORMANCE_RESPONSE_ID",
            serde_json::to_string(&result_probes()).unwrap(),
        );
        if !self.capture_live_status {
            let snapshot = legacy_fixtures()["status_snapshot"].clone();
            if !snapshot.is_null() {
                command.env("AFT_TEST_CONFORMANCE_STATUS_SNAPSHOT", snapshot.to_string());
            }
        }
        let child = command.spawn().map_err(|error| {
            HarnessError::new(format!(
                "launching {} with --subc {} in cwd {} (exe_exists={}, cwd_exists={}): {error}",
                command.get_program().to_string_lossy(),
                connection_path.display(),
                root.display(),
                Path::new(command.get_program()).is_file(),
                root.is_dir()
            ))
        })?;
        // Own the child before awaiting the handshake so failed setup also reaps it.
        struct ChildGuard(Option<Child>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if let Some(child) = &mut self.0 {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let mut guard = ChildGuard(Some(child));
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .map_err(harness_error)?
            .map_err(harness_error)?;
        authenticate_server(
            &mut stream,
            &[0x42; subc_transport::KEY_LEN],
            &[0x24; subc_transport::DAEMON_ID_LEN],
            "tool-provider-conformance",
            Duration::from_secs(5),
        )
        .await
        .map_err(harness_error)?;
        let hello = next_frame(&mut stream).await?;
        if hello.header.ty != FrameType::Hello {
            return Err(HarnessError::new("expected module Hello"));
        }
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: vec![],
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        };
        write_frame(
            &mut stream,
            &Frame::build(
                FrameType::HelloAck,
                flags(),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&ack).unwrap(),
            )
            .map_err(harness_error)?,
        )
        .await
        .map_err(harness_error)?;
        let (read, write) = stream.into_split();
        let (unclaimed_tx, unclaimed_rx) = mpsc::unbounded_channel();
        let wire = Arc::new(Wire {
            writer: AsyncMutex::new(write),
            unclaimed: AsyncMutex::new(unclaimed_rx),
            waiters: Mutex::default(),
            next_corr: AtomicU64::new(100),
            next_channel: AtomicU16::new(1),
            last_exchange_outcome: Mutex::default(),
        });
        let reader = spawn_reader(read, wire.clone(), unclaimed_tx, self.stand_in.clone());
        Ok(Process {
            child: Mutex::new(guard.0.take().unwrap()),
            wire,
            reader,
            root: root.to_path_buf(),
        })
    }
    async fn route(&self, handle: &Process, stamp: &RouteStamp) -> Result<Route, HarnessError> {
        let mut route = bind_route_stamped(
            handle,
            &handle.root.join("project"),
            "runner",
            "conformance-session",
            Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
            stamp,
        )
        .await?;
        route.scoped_preset = self.preset.or(Some("head"));
        route.legacy_diagnostics = Some(self.legacy_diagnostics.clone());
        wait_for_route_ready(&route, self.quick_call()).await;
        if self.served_tools.lock().unwrap().is_none() {
            let catalog = catalog_reply(&route, self.catalog_arguments()).await;
            *self.served_tools.lock().unwrap() = Some(
                catalog["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap().to_owned())
                    .collect(),
            );
        }
        if self.catalog_powershell {
            assert_eq!(
                self.served_tools
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .contains("powershell"),
                self.preset != Some("reader"),
                "catalog-only pwsh fixture must produce the requested advertised-tool shape"
            );
        }
        Ok(route)
    }
    async fn kill_at(
        &self,
        handle: Process,
        point: &KillPoint,
        trigger: Trigger<'_>,
    ) -> Result<KillReport, HarnessError> {
        #[cfg(not(feature = "test-timing-hooks"))]
        {
            let _ = (handle, point, trigger);
            Err(HarnessError::new("kill hooks require test-timing-hooks"))
        }
        #[cfg(feature = "test-timing-hooks")]
        {
            let name = if point == &KillPoint::new("Prepared") {
                "Prepared"
            } else if point == &KillPoint::new("Authorized") {
                "Authorized"
            } else {
                return Err(HarnessError::new("unsupported kill point"));
            };
            let signal = handle.root.join("ledger-signal");
            let _ = std::fs::remove_file(&signal);
            std::fs::write(
                handle.root.join("ledger-control.json"),
                serde_json::to_vec(&json!({"point":name,"signal_path":signal})).unwrap(),
            )
            .map_err(harness_error)?;
            *handle.wire.last_exchange_outcome.lock().unwrap() = None;
            wait_for_kill_signal(&signal, trigger, &handle.wire.last_exchange_outcome).await?;
            let mut child = handle.child.lock().unwrap();
            child.kill().map_err(harness_error)?;
            child.wait().map_err(harness_error)?;
            Ok(KillReport {
                point: point.clone(),
                mechanism: cortexkit_role_harness::KillMechanism::FaultHookThenProcessKill,
            })
        }
    }
    async fn restart(&self, root: &Path) -> Result<Process, HarnessError> {
        let _ = std::fs::remove_file(root.join("ledger-control.json"));
        self.spawn(root).await
    }
}

#[cfg(feature = "test-timing-hooks")]
async fn wait_for_kill_signal(
    signal: &Path,
    trigger: Trigger<'_>,
    outcome: &Mutex<Option<String>>,
) -> Result<(), HarnessError> {
    let started = tokio::time::Instant::now();
    let reached = async {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !signal.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(harness_error)
    };
    tokio::select! {
        result = reached => result,
        _ = trigger => {
            // The marker can arrive while the observer's poll is asleep. A
            // completed trigger is an error only if the durable point is absent
            // at this recheck; do not wait for a later marker after completion.
            let marker_present = signal.exists();
            if marker_present {
                return Ok(());
            }
            let message = format!(
                "trigger ended before durable kill point: marker_present={}, elapsed={:?}, {}",
                marker_present,
                started.elapsed(),
                outcome.lock().unwrap().as_deref().unwrap_or("no route outcome recorded"),
            );
            // The inventory abbreviates case reasons, so also retain the full
            // terminal frame or transport error in the failed test's output.
            eprintln!("{message}");
            Err(HarnessError::new(message))
        }
    }
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test(start_paused = true)]
async fn kill_signal_written_while_poll_sleeps_wins_over_completed_trigger() {
    let dir = tempfile::tempdir().unwrap();
    let signal = dir.path().join("ledger-signal");
    let trigger: Trigger<'_> = Box::pin(async {
        // Paused time makes the trigger finish between the observer's first
        // absent-marker check and its next 10 ms poll, without a wall-clock race.
        tokio::time::sleep(Duration::from_millis(1)).await;
        std::fs::write(&signal, "Authorized").unwrap();
    });
    wait_for_kill_signal(&signal, trigger, &Mutex::default())
        .await
        .unwrap();
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test(start_paused = true)]
async fn completed_trigger_without_kill_signal_still_fails_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let signal = dir.path().join("ledger-signal");
    let started = tokio::time::Instant::now();
    let error = wait_for_kill_signal(&signal, Box::pin(async {}), &Mutex::default())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(
        error.contains("trigger ended before durable kill point"),
        "{error}"
    );
    assert!(error.contains("marker_present=false"), "{error}");
    assert!(error.contains("elapsed=0ns"), "{error}");
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn premature_trigger_diagnostics_include_response_error_and_eof() {
    for (terminal, expected) in [
        (
            Some((FrameType::Response, json!({"output":"early response"}))),
            "early response",
        ),
        (
            Some((
                FrameType::Error,
                json!({"code":"unknown_tool","message":"early refusal"}),
            )),
            "unknown_tool",
        ),
        (None, "module EOF"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let signal = dir.path().join("ledger-signal");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let (read, write) = stream.into_split();
        let (unclaimed_tx, unclaimed_rx) = mpsc::unbounded_channel();
        let wire = Arc::new(Wire {
            writer: AsyncMutex::new(write),
            unclaimed: AsyncMutex::new(unclaimed_rx),
            waiters: Mutex::default(),
            next_corr: AtomicU64::new(100),
            next_channel: AtomicU16::new(1),
            last_exchange_outcome: Mutex::default(),
        });
        let reader = spawn_reader(read, wire.clone(), unclaimed_tx, Arc::default());
        let route = Route {
            wire: wire.clone(),
            channel: 1,
            scoped_preset: None,
            legacy_diagnostics: None,
        };
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let server = async move {
            let request = next_frame(&mut peer).await.unwrap();
            if let Some((ty, body)) = terminal {
                let reply = Frame::build(
                    ty,
                    flags(),
                    1,
                    1,
                    request.header.corr,
                    serde_json::to_vec(&body).unwrap(),
                )
                .unwrap();
                write_frame(&mut peer, &reply).await.unwrap();
                // Keep the transport alive through the route's terminal-frame
                // quiet window; only the EOF case closes without a reply.
                let _ = done_rx.await;
            }
        };
        let trigger: Trigger<'_> = Box::pin(async {
            tokio::join!(
                async {
                    let _ = route.request(json!({"name":"read","arguments":{}})).await;
                    let _ = done_tx.send(());
                },
                server
            );
        });
        let error = wait_for_kill_signal(&signal, trigger, &wire.last_exchange_outcome)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
        assert!(error.contains("marker_present=false"), "{error}");
        assert!(error.contains("elapsed="), "{error}");
        reader.await.unwrap();
    }
}

async fn ledger_database(root: &Path) -> PathBuf {
    fn visit(path: &Path) -> Option<PathBuf> {
        for entry in std::fs::read_dir(path).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = visit(&path) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|name| name == "aft.db") {
                return Some(path);
            }
        }
        None
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(path) = visit(root) {
            // File creation and the bash tables precede the call-ledger migration.
            // A refused call need not wait for it, so observe the ledger itself.
            if let Ok(conn) = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                conn.busy_timeout(Duration::ZERO).unwrap();
                if conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'call_ledger')",
                    [], |row| row.get::<_, bool>(0),
                ).unwrap_or(false) {
                    return path;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "waited 30 seconds for the call_ledger migration in the module's aft.db under {}",
            root.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(debug_assertions)]
struct DatabaseOpenGate(PathBuf);

#[cfg(debug_assertions)]
impl DatabaseOpenGate {
    fn hold(root: &Path) -> Self {
        std::fs::create_dir_all(root).unwrap();
        let path = root.join("database-open-gate");
        std::fs::write(&path, b"hold").unwrap();
        Self(path)
    }

    async fn entered(&self) {
        wait_for_file(&self.0.with_extension("entered")).await;
    }
}

#[cfg(debug_assertions)]
impl Drop for DatabaseOpenGate {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn scoped_stamp(carrier: &str, owner: &str, scope_ref: &str) -> RouteStamp {
    RouteStamp {
        principal: format!("reserved:{carrier}"),
        scope: Some(cortexkit_role_harness::ScopeStamp {
            owner: format!("reserved:{owner}"),
            scope_ref: scope_ref.into(),
            scope_epoch: 7,
        }),
    }
}

fn marker_command(marker: &Path) -> String {
    format!(
        "printf ran > '{}'",
        marker.display().to_string().replace('\'', "'\\''")
    )
}

fn response_json(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap()
}

fn assert_approved_scoped_shell_result(frame: &Frame, marker: &Path) {
    assert_eq!(frame.header.ty, FrameType::Response);
    let result = response_json(frame);
    if cfg!(windows) {
        // Elicitation authorizes the command, not an escape from strict confinement.
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(
            result["content"][0]["text"],
            "sandbox profile strict is not enforced on Windows; select sandbox.profile off or run on macOS/Linux"
        );
        assert!(!marker.exists(), "a refused command must not run");
    } else {
        assert_eq!(result["isError"], false, "{result}");
        assert!(marker.exists(), "the approved command must run");
    }
}

fn is_database_initializing(frame: &Frame) -> bool {
    response_json(frame)["structuredContent"]["code"] == "database_initializing"
        || String::from_utf8_lossy(&frame.body)
            .contains("Project persistence is still initializing")
}

async fn wait_for_route_ready(route: &Route, probe: CallSpec) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (name, arguments) = probe.clone();
        let frame = tokio::time::timeout_at(
            deadline,
            route.terminal(json!({"name":name,"arguments":arguments})),
        )
        .await
        .unwrap_or_else(|_| panic!("project persistence did not become ready within 15 seconds"));
        assert_eq!(
            frame.header.ty,
            FrameType::Response,
            "route readiness probe did not return a response: {}",
            String::from_utf8_lossy(&frame.body)
        );
        if !is_database_initializing(&frame) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "project persistence did not become ready within 15 seconds: {}",
            String::from_utf8_lossy(&frame.body)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn s1_keyed_replay_headers_bytes_expiry_and_keyless_legacy_exclusion() {
    use aft::db::call_ledger::{self as ledger, Key};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    std::fs::write(root.join("project/input.txt"), "first content\n").unwrap();
    let body = json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"replay"});
    let first = route.raw(body.clone(), false).await;
    assert_eq!(first.header.ty, FrameType::Response);
    assert_eq!(response_json(&first)["isError"], false);
    std::fs::write(root.join("project/input.txt"), "changed content\n").unwrap();
    let repeat = route.raw(body.clone(), false).await;
    assert_ne!(first.header.corr, repeat.header.corr);
    assert_eq!(first.header.ty, repeat.header.ty);
    assert_eq!(first.body, repeat.body);
    let other = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    let repeat = other.raw(body.clone(), false).await;
    assert_ne!(first.header.channel, repeat.header.channel);
    assert_eq!(first.body, repeat.body);
    let conflict = route
        .raw(
            json!({"name":"read","arguments":{"filePath":"missing.txt"},"call_key":"replay"}),
            false,
        )
        .await;
    assert_eq!(conflict.header.ty, FrameType::Error);
    assert_eq!(response_json(&conflict)["code"], "invalid_request");
    assert_eq!(response_json(&conflict)["detail"]["field"], "call_key");
    let plain = route
        .raw(
            json!({"name":"read","arguments":{"filePath":"input.txt"}}),
            false,
        )
        .await;
    assert!(String::from_utf8_lossy(&plain.body).contains("changed content"));
    let legacy = bind_route_declaring(&process, &root.join("project"), "runner", "legacy", None)
        .await
        .unwrap();
    assert_eq!(
        legacy
            .raw(
                json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"legacy"}),
                false
            )
            .await
            .header
            .ty,
        FrameType::Response
    );
    drop(process);
    // Direct database assertions and forced clock changes are offline: tests
    // never open another SQLite handle while the actor owns this database.
    let path = ledger_database(&root).await;
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM call_ledger", [], |r| r
            .get::<_, usize>(0))
            .unwrap(),
        1
    );
    let key = Key {
        carrier: "direct".into(),
        call_key: "replay".into(),
    };
    let row = ledger::get(&conn, &key).unwrap().unwrap();
    assert!(row.scope.is_none());
    assert!(row.late_entry.is_none());
    assert_eq!(row.frame.unwrap().body, first.body);
    conn.execute(
        "UPDATE call_ledger SET settled_at=?1",
        [ledger::now_ms() - ledger::RETENTION_MS],
    )
    .unwrap();
    // An unscoped row expires by deletion, so expiry replay is covered on a
    // scoped row below rather than confusing deletion with retained identity.
    drop(conn);
    let process = Subject::head().restart(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&route.raw(body, false).await.body).contains("changed content"));
}

#[tokio::test]
async fn s1_scoped_owner_identity_conflicts_and_expired_replay() {
    use aft::db::call_ledger::{self as ledger, Key};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    std::fs::write(root.join("project/input.txt"), "content\n").unwrap();
    let stamp = scoped_stamp("carrier-a", "owner-a", "same-ref");
    let route = Subject::head().route(&process, &stamp).await.unwrap();
    let body = json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"scoped"});
    let first = route.raw(body.clone(), false).await;
    assert_eq!(response_json(&first)["isError"], false);
    let other = Subject::head().route(&process, &stamp).await.unwrap();
    let repeat = other.raw(body.clone(), false).await;
    assert_eq!(first.body, repeat.body);
    assert_eq!(first.header.ty, repeat.header.ty);
    for conflict_stamp in [
        scoped_stamp("carrier-a", "owner-b", "same-ref"),
        scoped_stamp("carrier-a", "owner-a", "different-ref"),
        RouteStamp {
            scope: Some(cortexkit_role_harness::ScopeStamp {
                scope_epoch: 8,
                ..stamp.scope.clone().unwrap()
            }),
            ..stamp.clone()
        },
        RouteStamp {
            principal: stamp.principal.clone(),
            scope: None,
        },
    ] {
        let other = Subject::head()
            .route(&process, &conflict_stamp)
            .await
            .unwrap();
        let refused = other.raw(body.clone(), false).await;
        assert_eq!(refused.header.ty, FrameType::Error);
        assert_eq!(response_json(&refused)["detail"]["field"], "call_key");
    }
    let stamp_b = scoped_stamp("carrier-b", "owner-b", "same-ref");
    let other = Subject::head().route(&process, &stamp_b).await.unwrap();
    assert_eq!(
        response_json(&other.raw(body.clone(), false).await)["isError"],
        false
    );
    let trusted = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    let missing = json!({"name":"read","arguments":{"filePath":"missing.txt"}});
    let native = response_json(&trusted.raw(missing.clone(), false).await)["structuredContent"]
        ["code"]
        .as_str()
        .unwrap()
        .to_string();
    let mut missing_keyed = missing;
    missing_keyed["call_key"] = json!("error-replay");
    let error = route.raw(missing_keyed.clone(), false).await;
    assert_eq!(response_json(&error)["isError"], true);
    assert!(response_json(&error).get("structuredContent").is_none());
    drop(process);
    let path = ledger_database(&root).await;
    let conn = rusqlite::Connection::open(&path).unwrap();
    for (carrier, owner) in [("carrier-a", "owner-a"), ("carrier-b", "owner-b")] {
        let row = ledger::get(
            &conn,
            &Key {
                carrier: format!("reserved:{carrier}"),
                call_key: "scoped".into(),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(row.scope.unwrap().owner, format!("reserved:{owner}"));
        assert_eq!(
            serde_json::from_str::<Value>(row.late_entry.as_deref().unwrap()).unwrap()["owner"],
            format!("reserved:{owner}")
        );
    }
    conn.execute(
        "UPDATE call_ledger SET settled_at=?1",
        [ledger::now_ms() - ledger::RETENTION_MS],
    )
    .unwrap();
    drop(conn);
    let process = Subject::head().restart(&root).await.unwrap();
    let route = Subject::head().route(&process, &stamp).await.unwrap();
    let expired = route.raw(body, false).await;
    assert_eq!(expired.header.ty, FrameType::Error);
    let expired = response_json(&expired);
    assert_eq!(expired["code"], "result_not_retained");
    assert_eq!(expired["detail"]["outcome"], "ok");
    let expired_error = response_json(&route.raw(missing_keyed, false).await);
    assert_eq!(expired_error["detail"]["outcome"], native);
}

#[tokio::test]
async fn s1_database_unavailable_refuses_keyed_read_and_shell_but_not_keyless_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    std::fs::write(root.join("project/input.txt"), "readable\n").unwrap();
    route
        .raw(
            json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"seed"}),
            false,
        )
        .await;
    drop(process);
    let path = ledger_database(&root).await;
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("DELETE FROM call_ledger", []).unwrap();
    conn.execute(
        "UPDATE schema_version SET version=?1",
        [aft::db::CURRENT_SCHEMA_VERSION + 1],
    )
    .unwrap();
    drop(conn);
    let process = Subject::head().restart(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    let marker = root.join("project/should-not-run");
    for body in [
        json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"blocked-read"}),
        json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"blocked-bash"}),
    ] {
        let frame = route.raw(body, false).await;
        let response = response_json(&frame);
        assert_eq!(
            response["structuredContent"]["code"], "database_unavailable",
            "{response}"
        );
        // The persistence gate retries opening on the next call. Refusal occurs
        // before durable admission or execution, so retrying cannot duplicate work.
        assert_eq!(response["structuredContent"]["retryable"], true);
    }
    let plain = route
        .raw(
            json!({"name":"read","arguments":{"filePath":"input.txt"}}),
            false,
        )
        .await;
    assert!(String::from_utf8_lossy(&plain.body).contains("readable"));
    assert!(!marker.exists());
    drop(process);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM call_ledger", [], |r| r
            .get::<_, usize>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn s1_keyless_untrusted_shell_keeps_slice_a_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let marker = root.join("project/keyless-marker");
    let reply = route
        .raw(
            json!({"name":"bash","arguments":{"command":marker_command(&marker)}}),
            true,
        )
        .await;
    assert_eq!(reply.header.ty, FrameType::Error);
    assert_eq!(response_json(&reply)["code"], "capability_not_admitted");
    assert!(!marker.exists());
}

// The database-open gate exists only in debug builds of the daemon
// (`configure::open_database_runtime`), so this test does too.
#[cfg(debug_assertions)]
#[tokio::test]
async fn s1_keyed_write_during_migration_fails_closed_then_retries_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let gate = DatabaseOpenGate::hold(&root);
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    gate.entered().await;
    let marker = root.join("project/keyed-write.txt");
    let body = json!({"name":"write","arguments":{"filePath":"keyed-write.txt","content":"first execution"},"call_key":"migration-write"});
    let refused = route.terminal(body.clone()).await;
    assert_eq!(refused.header.ty, FrameType::Response);
    let refusal = response_json(&refused);
    assert_eq!(refusal["isError"], true, "{refusal}");
    assert_eq!(
        refusal["structuredContent"]["code"], "database_initializing",
        "{refusal}"
    );
    assert_eq!(refusal["structuredContent"]["retryable"], true, "{refusal}");
    assert!(
        !marker.exists(),
        "a keyed write cannot run before ledger readiness"
    );
    drop(gate);
    ledger_database(&root).await;
    // The schema commits before the actor publishes its database handle.
    let first = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let frame = route.terminal(body.clone()).await;
            if !is_database_initializing(&frame) {
                break frame;
            }
            assert!(!marker.exists(), "a refused retry cannot execute the write");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("keyed write admission did not become ready within 30 seconds");
    assert_eq!(first.header.ty, FrameType::Response);
    assert_eq!(
        response_json(&first)["isError"],
        false,
        "{}",
        response_json(&first)
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "first execution");
    std::fs::write(&marker, "after execution").unwrap();
    let repeat = route.terminal(body).await;
    assert_eq!(repeat.header.ty, first.header.ty);
    assert_eq!(repeat.body, first.body);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "after execution",
        "replay must not execute the write twice"
    );
    drop(process);
    let conn = rusqlite::Connection::open(ledger_database(&root).await).unwrap();
    let (rows, state): (usize, String) = conn
        .query_row(
            "SELECT COUNT(*), state FROM call_ledger WHERE call_key = 'migration-write'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        rows, 1,
        "the executed keyed write must have exactly one durable admission"
    );
    assert_eq!(state, "Settled");
}

#[tokio::test]
async fn s1_untrusted_keyed_shell_without_elicitation_is_refused_at_admission() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    // Debug daemons honour the open gate, which holds the migration so the
    // database assertions below must wait for readiness. Release daemons
    // ignore it, and the readiness wait still covers the race.
    #[cfg(debug_assertions)]
    let gate = DatabaseOpenGate::hold(&root);
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = bind_route_stamped_with_elicitation(
        &process,
        &root.join("project"),
        "runner",
        "conformance-session",
        Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
        &scoped_stamp("carrier", "owner", "scope"),
        false,
    )
    .await
    .unwrap();
    let marker = root.join("project/no-elicitation-marker");
    let reply = route
        .raw(
            json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"no-elicitation"}),
            false,
        )
        .await;
    assert_eq!(reply.header.ty, FrameType::Error);
    assert_eq!(response_json(&reply)["code"], "capability_not_admitted");
    assert!(!marker.exists());
    #[cfg(debug_assertions)]
    let release = {
        gate.entered().await;
        // Admission refusal intentionally precedes persistence readiness. Leave
        // the migration held briefly so database assertions must observe readiness.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(750)).await;
            drop(gate);
        })
    };
    let conn = rusqlite::Connection::open(ledger_database(&root).await).unwrap();
    let executions: usize = conn
        .query_row("SELECT COUNT(*) FROM bash_tasks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(executions, 0);
    let admissions: usize = conn
        .query_row("SELECT COUNT(*) FROM call_ledger", [], |row| row.get(0))
        .unwrap();
    assert_eq!(admissions, 0);
    #[cfg(debug_assertions)]
    release.await.unwrap();
}

#[tokio::test]
async fn s1_untrusted_keyed_shell_denial_and_elicitation_errors_never_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let warm = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"read","arguments":{"path":"input.txt"}}),
        false,
    )
    .await;
    let route = Subject::head()
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    for (case, ty, body) in [
        (
            "deny",
            FrameType::Response,
            br#"{"decision":"deny"}"#.to_vec(),
        ),
        ("malformed", FrameType::Response, b"not JSON".to_vec()),
        (
            "failed",
            FrameType::Error,
            br#"{"code":"elicitation_failed"}"#.to_vec(),
        ),
    ] {
        let marker = root.join(format!("project/{case}-marker"));
        let mut call =
            json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":case});
        route.adapt_preset(&mut call);
        let mut wire = route.wire.lock().await;
        let corr = wire.corr();
        wire.send(
            &Frame::build(
                FrameType::Request,
                flags(),
                route.channel,
                1,
                corr,
                serde_json::to_vec(&call).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ask = wire.next().await.unwrap();
        assert_eq!(ask.header.ty, FrameType::Request, "{case}: {ask:?}");
        assert_eq!(ask.header.channel, route.channel);
        assert!(!marker.exists(), "{case}: execution must wait for a grant");
        wire.send(&Frame::build(ty, flags(), route.channel, 1, ask.header.corr, body).unwrap())
            .await
            .unwrap();
        let reply = wire.next().await.unwrap();
        assert_eq!(reply.header.corr, corr, "{case}");
        assert_eq!(reply.header.ty, FrameType::Response, "{case}");
        assert_eq!(response_json(&reply)["isError"], true, "{case}");
        assert!(
            !marker.exists(),
            "{case}: refusal must never spawn the shell"
        );
    }
    let conn = rusqlite::Connection::open(ledger_database(&root).await).unwrap();
    let executions: usize = conn
        .query_row("SELECT COUNT(*) FROM bash_tasks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(executions, 0);
    let refusals: usize = conn
        .query_row("SELECT COUNT(*) FROM call_ledger WHERE state = 'Settled' AND outcome = 'bash_denied_untrusted'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(refusals, 3);
}

#[tokio::test]
async fn s1_cancelled_prepared_repeat_ends_without_settling_the_execution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let warm = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let marker = root.join("project/cancelled-repeat");
    let mut body = json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"cancelled-repeat"});
    route.adapt_preset(&mut body);
    let mut wire = route.wire.lock().await;
    let first = wire.corr();
    let repeat = wire.corr();
    wire.send(
        &Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            first,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let ask = wire.next().await.unwrap();
    assert_eq!(ask.header.ty, FrameType::Request);
    wire.send(
        &Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            repeat,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    wire.send(&Frame::build(FrameType::Cancel, flags(), route.channel, 1, repeat, vec![]).unwrap())
        .await
        .unwrap();
    let cancelled = tokio::time::timeout(Duration::from_secs(5), wire.next())
        .await
        .expect("a cancelled attachment must receive a terminal frame")
        .unwrap();
    assert_eq!(cancelled.header.corr, repeat);
    assert_eq!(cancelled.header.ty, FrameType::Error);
    assert_eq!(response_json(&cancelled)["code"], "cancelled");
    assert!(!marker.exists());
    wire.send(
        &Frame::build(
            FrameType::Response,
            flags(),
            route.channel,
            1,
            ask.header.corr,
            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let completed = wire.next().await.unwrap();
    assert_eq!(completed.header.corr, first);
    assert_approved_scoped_shell_result(&completed, &marker);
    assert!(
        tokio::time::timeout(Duration::from_millis(400), wire.next())
            .await
            .is_err(),
        "the cancelled repeat must not also receive the result"
    );
    drop(wire);
    let replay = route.raw(body, false).await;
    assert_eq!(replay.body, completed.body);
    if !cfg!(windows) {
        assert_eq!(std::fs::read(marker).unwrap(), b"ran");
    }
}

#[tokio::test]
async fn s1_prepared_and_running_repeats_attach_without_second_question_or_execution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let warm = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let marker = root.join("project/executions");
    let mut body = json!({"name":"bash","arguments":{"command":format!("printf x >> '{}'; sleep 1",marker.display())},"call_key":"held-repeat"});
    route.adapt_preset(&mut body);
    let mut wire = route.wire.lock().await;
    let first = wire.corr();
    let send = |corr| {
        Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    };
    wire.send(&send(first)).await.unwrap();
    let ask = wire.next().await.unwrap();
    assert_eq!(
        ask.header.ty,
        FrameType::Request,
        "{}",
        String::from_utf8_lossy(&ask.body)
    );
    let held = wire.corr();
    wire.send(&send(held)).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), wire.next())
            .await
            .is_err(),
        "a Prepared repeat must not file a second question"
    );
    wire.send(
        &Frame::build(
            FrameType::Response,
            flags(),
            route.channel,
            1,
            ask.header.corr,
            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let mut replies = BTreeMap::new();
    if !cfg!(windows) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    let repeat = wire.corr();
    wire.send(&send(repeat)).await.unwrap();
    for _ in 0..3 {
        let frame = wire.next().await.unwrap();
        assert_approved_scoped_shell_result(&frame, &marker);
        replies.insert(frame.header.corr, frame.body);
    }
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[&first], replies[&held]);
    assert_eq!(replies[&first], replies[&repeat]);
    if !cfg!(windows) {
        assert_eq!(std::fs::read(&marker).unwrap(), b"x");
    }
    drop(wire);
    drop(process);
    let conn = rusqlite::Connection::open(ledger_database(&root).await).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM call_ledger WHERE late_entry IS NOT NULL",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn s1_cross_actor_prepared_repeat_does_not_run_startup_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let stamp = scoped_stamp("carrier", "owner", "scope");
    let route = Subject::head().route(&process, &stamp).await.unwrap();
    let warm = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let marker = root.join("project/cross-actor");
    let mut body = json!({"name":"bash","arguments":{"command":format!("printf x >> '{}'",marker.display())},"call_key":"cross-actor"});
    route.adapt_preset(&mut body);
    let mut wire = route.wire.lock().await;
    let first = wire.corr();
    wire.send(
        &Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            first,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let ask = wire.next().await.unwrap();
    assert_eq!(ask.header.ty, FrameType::Request);
    drop(wire);
    let second_root = root.join("project2");
    std::fs::create_dir_all(second_root.join(".cortexkit")).unwrap();
    std::fs::copy(
        root.join("project/.cortexkit/aft.jsonc"),
        second_root.join(".cortexkit/aft.jsonc"),
    )
    .unwrap();
    let second_warm = bind_route(&process, &second_root, "runner", "second-root")
        .await
        .unwrap();
    second_warm
        .raw(
            json!({"name":"bash","arguments":{"command":"printf warm"}}),
            false,
        )
        .await;
    let second = bind_route_stamped(
        &process,
        &second_root,
        "runner",
        "conformance-session",
        Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
        &stamp,
    )
    .await
    .unwrap();
    let mut wire = route.wire.lock().await;
    let repeat = wire.corr();
    wire.send(
        &Frame::build(
            FrameType::Request,
            flags(),
            second.channel,
            1,
            repeat,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    wire.send(
        &Frame::build(
            FrameType::Response,
            flags(),
            route.channel,
            1,
            ask.header.corr,
            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let mut replies = BTreeMap::new();
    for _ in 0..2 {
        let frame = wire.next().await.unwrap();
        assert_eq!(
            frame.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&frame.body)
        );
        assert_approved_scoped_shell_result(&frame, &marker);
        replies.insert(frame.header.corr, frame.body);
    }
    assert_eq!(replies[&first], replies[&repeat]);
    if !cfg!(windows) {
        assert_eq!(std::fs::read(marker).unwrap(), b"x");
    }
}

#[cfg(feature = "test-timing-hooks")]
async fn crash_before_dispatch(point: &str, shell: &str) {
    use aft::db::call_ledger::{self as ledger, Key, State};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let stamp = scoped_stamp("carrier", "owner", "scope");
    let route = Subject::head().route(&process, &stamp).await.unwrap();
    let marker = root.join("project/crash-marker");
    let command = if shell == "powershell" {
        format!(
            "Set-Content -LiteralPath '{}' -Value ran",
            marker.display().to_string().replace('\'', "''")
        )
    } else {
        marker_command(&marker)
    };
    let body = json!({"name":shell,"arguments":{"command":command},"call_key":"crash"});
    let trigger: Trigger<'_> = Box::pin(async {
        let frame = route.raw(body, true).await;
        panic!(
            "call returned before kill point: {}",
            String::from_utf8_lossy(&frame.body)
        );
    });
    let report = Subject::head()
        .kill_at(process, &KillPoint::new(point), trigger)
        .await
        .unwrap();
    assert_eq!(report.point, KillPoint::new(point));
    assert_eq!(
        report.mechanism,
        cortexkit_role_harness::KillMechanism::FaultHookThenProcessKill
    );
    let path = ledger_database(&root).await;
    let conn = rusqlite::Connection::open(&path).unwrap();
    let key = Key {
        carrier: "reserved:carrier".into(),
        call_key: "crash".into(),
    };
    let row = ledger::get(&conn, &key).unwrap().unwrap();
    assert_eq!(
        row.state,
        if point == "Prepared" {
            State::Prepared
        } else {
            State::Authorized
        }
    );
    assert!(row.task_id.is_none());
    drop(conn);
    let process = Subject::head().restart(&root).await.unwrap();
    let route = Subject::head().route(&process, &stamp).await.unwrap();
    let replay = route
        .raw(
            json!({"name":shell,"arguments":{"command":command},"call_key":"crash"}),
            false,
        )
        .await;
    assert_eq!(replay.header.ty, FrameType::Error);
    assert_eq!(
        response_json(&replay)["detail"]["reason"],
        "restart_before_dispatch"
    );
    assert!(!marker.exists());
    drop(process);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let row = ledger::get(&conn, &key).unwrap().unwrap();
    assert_eq!(row.state, State::Settled);
    assert_eq!(row.seq, Some(1));
    let entry: Value = serde_json::from_str(row.late_entry.as_deref().unwrap()).unwrap();
    assert_eq!(entry["kind"], "not_started");
    assert_eq!(entry["reason"], "restart_before_dispatch");
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM call_ledger WHERE seq IS NOT NULL",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn s1_crash_at_prepared_restarts_not_started() {
    crash_before_dispatch("Prepared", "bash").await;
}
#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn s1_crash_at_authorized_restarts_not_started() {
    crash_before_dispatch("Authorized", "bash").await;
}

#[cfg(all(windows, feature = "test-timing-hooks"))]
#[tokio::test]
async fn s1_powershell_crash_at_prepared_restarts_not_started() {
    crash_before_dispatch("Prepared", "powershell").await;
}

#[tokio::test]
async fn s1_unarmed_shell_commits_both_held_states_and_replays_before_injections() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let marker = root.join("project/marker");
    assert!(!root.join("ledger-control.json").exists());
    let body =
        json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"unarmed"});
    let first = route.raw(body.clone(), true).await;
    assert_approved_scoped_shell_result(&first, &marker);
    if !cfg!(windows) {
        std::fs::remove_file(&marker).unwrap();
    }
    let repeat = route.raw(body, false).await;
    assert_eq!(first.body, repeat.body);
    assert!(!marker.exists());
}

#[tokio::test]
async fn s1_restart_recovers_running_shell_once_and_reduces_unobserved_read() {
    restart_recovers_shell_once(false).await;
}

#[cfg(all(unix, feature = "test-timing-hooks"))]
#[tokio::test]
async fn s1_restart_recovers_child_created_before_running_record() {
    restart_recovers_shell_once(true).await;
}

async fn restart_recovers_shell_once(hold_running_record: bool) {
    use aft::db::call_ledger::{self as ledger, Key, ScopeIdentity, State};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let stamp = scoped_stamp("carrier", "owner", "scope");
    let route = Subject::head().route(&process, &stamp).await.unwrap();
    let warm = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let started = root.join("project/started");
    let finished = root.join("project/finished");
    let release = root.join("project/release-recovered-output");
    struct ReleaseOutput(PathBuf);
    impl Drop for ReleaseOutput {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"release");
        }
    }
    let _release_on_failure = ReleaseOutput(release.clone());
    let gate = root.join("after-create-gate");
    let wait = if hold_running_record {
        std::fs::write(&gate, b"hold").unwrap();
        format!(
            "while [ ! -f '{}' ]; do sleep 0.01; done",
            release.display().to_string().replace('\'', "'\\''")
        )
    } else {
        "sleep 2".to_string()
    };
    let command = format!(
        "{}; {wait}; printf recovered-output; {}",
        marker_command(&started),
        marker_command(&finished)
    );
    let mut body =
        json!({"name":"bash","arguments":{"command":command},"call_key":"recover-shell"});
    route.adapt_preset(&mut body);
    let mut wire = route.wire.lock().await;
    let corr = wire.corr();
    wire.send(
        &Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let ask = wire.next().await.unwrap();
    assert_eq!(ask.header.ty, FrameType::Request);
    wire.send(
        &Frame::build(
            FrameType::Response,
            flags(),
            route.channel,
            1,
            ask.header.corr,
            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    if cfg!(windows) {
        // There can be no running child to recover without a Windows sandbox backend.
        // The terminal refusal must instead survive restart and replay without asking again.
        let refused = wire.next().await.unwrap();
        assert_eq!(refused.header.corr, corr);
        assert_approved_scoped_shell_result(&refused, &started);
        assert!(!finished.exists());
        drop(wire);
        drop(process);
        let process = Subject::head().restart(&root).await.unwrap();
        let route = Subject::head().route(&process, &stamp).await.unwrap();
        let replay = route.raw(body, false).await;
        assert_eq!(replay.body, refused.body);
        assert_approved_scoped_shell_result(&replay, &started);
        assert!(!finished.exists());
        let conn = rusqlite::Connection::open(ledger_database(&root).await).unwrap();
        let key = Key {
            carrier: "reserved:carrier".into(),
            call_key: "recover-shell".into(),
        };
        assert_eq!(
            ledger::get(&conn, &key).unwrap().unwrap().state,
            State::Settled
        );
        return;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let checkpoint = if hold_running_record {
        wait_for_file(&gate.with_extension("held")).await;
        let checkpoint: Value =
            serde_json::from_slice(&std::fs::read(gate.with_extension("held")).unwrap()).unwrap();
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(checkpoint["metadata_path"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["status"], "starting");
        assert!(aft::bash_background::process::is_process_alive(
            checkpoint["child_pid"].as_u64().unwrap() as u32
        ));
        eprintln!("live child before parent kill: {checkpoint}; persisted task: {metadata}");
        Some(checkpoint)
    } else {
        None
    };
    drop(wire);
    drop(process);
    let path = ledger_database(&root).await;
    let conn = rusqlite::Connection::open(&path).unwrap();
    let shell_key = Key {
        carrier: "reserved:carrier".into(),
        call_key: "recover-shell".into(),
    };
    assert_eq!(
        ledger::get(&conn, &shell_key).unwrap().unwrap().state,
        State::DispatchStarted
    );
    let read_key = Key {
        carrier: "reserved:reader".into(),
        call_key: "unknown-read".into(),
    };
    let read_body = json!({"name":"read","schema_pin":null,"arguments":{"filePath":"missing.txt"}});
    let digest = cortexkit_role_tool_provider::catalog::composition_digest(&read_body).unwrap();
    ledger::admit(
        &conn,
        &read_key,
        &digest,
        Some(&ScopeIdentity {
            owner: "reserved:owner".into(),
            scope_ref: "scope".into(),
            scope_epoch: 7,
        }),
        State::DispatchStarted,
    )
    .unwrap();
    drop(conn);
    let process = Subject::head().restart(&root).await.unwrap();
    let route = Subject::head().route(&process, &stamp).await.unwrap();
    let response = if let Some(checkpoint) = checkpoint {
        let task_path = Path::new(checkpoint["metadata_path"].as_str().unwrap());
        let recovered = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let metadata: Value =
                    serde_json::from_slice(&std::fs::read(task_path).unwrap()).unwrap();
                if metadata["status"] != "starting" {
                    break metadata;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("created child was not recovered");
        assert!(
            aft::bash_background::process::is_process_alive(
                checkpoint["child_pid"].as_u64().unwrap() as u32
            ),
            "detached child died with the old parent"
        );
        assert_eq!(
            recovered["status"], "running",
            "live child must remain running across restart: {recovered}"
        );
        let mut replay = route.start(body).await;
        std::fs::write(&release, b"release").unwrap();
        replay.terminal(Duration::from_secs(30)).await
    } else {
        route.raw(body, false).await
    };
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response_json(&response)["isError"], false);
    assert!(
        response_json(&response)["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("recovered-output"),
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    assert!(finished.exists());
    drop(process);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let shell = ledger::get(&conn, &shell_key).unwrap().unwrap();
    assert_eq!(shell.state, State::Settled);
    assert_eq!(
        serde_json::from_str::<Value>(shell.late_entry.as_deref().unwrap()).unwrap()["kind"],
        "result"
    );
    let read = ledger::get(&conn, &read_key).unwrap().unwrap();
    assert_eq!(read.state, State::Settled);
    assert_eq!(read.outcome.as_deref(), Some("unknown"));
    assert_eq!(
        serde_json::from_str::<Value>(read.late_entry.as_deref().unwrap()).unwrap()["outcome"],
        "unknown"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM call_ledger WHERE late_entry IS NOT NULL",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        2
    );
}
fn request_frame(channel: u16, corr: u64, body: &Value) -> Frame {
    Frame::build(
        FrameType::Request,
        flags(),
        channel,
        1,
        corr,
        serde_json::to_vec(body).unwrap(),
    )
    .unwrap()
}

impl Waiter {
    /// The request's terminal frame, exactly as the module sent it.
    async fn terminal(&mut self, within: Duration) -> Frame {
        loop {
            let frame = tokio::time::timeout(within, self.rx.recv())
                .await
                .expect("the request ended in time")
                .expect("the module is alive");
            if matches!(
                frame.header.ty,
                FrameType::Response | FrameType::Error | FrameType::StreamEnd
            ) {
                return frame;
            }
        }
    }
}

impl Route {
    /// Sends `body` without waiting; its frames go to the returned waiter.
    async fn start(&self, mut body: Value) -> Waiter {
        self.adapt_preset(&mut body);
        let corr = self.wire.corr();
        let waiter = self.wire.expect(self.channel, corr);
        self.wire
            .send(&request_frame(self.channel, corr, &body))
            .await
            .unwrap();
        waiter
    }
    /// Sends `body` and returns its terminal frame, bytes untouched.
    async fn terminal(&self, body: Value) -> Frame {
        self.start(body)
            .await
            .terminal(Duration::from_secs(30))
            .await
    }
}

fn withdraw_body(call_key: &str) -> Value {
    json!({"name": "tool.withdraw", "arguments": {"call_key": call_key}})
}

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} never appeared", path.display()));
}

/// Pulls `late_results` with `since: null` on `route`: every unacked entry
/// whose custodian is the route's principal, from the oldest.
async fn late_results_page(route: &Route) -> Value {
    let page = route
        .terminal(json!({"name": "late_results", "arguments": {"since": null}}))
        .await;
    assert_eq!(
        page.header.ty,
        FrameType::Response,
        "{}",
        String::from_utf8_lossy(&page.body)
    );
    response_json(&page)
}

fn entries_for<'a>(page: &'a Value, call_key: &str) -> Vec<&'a Value> {
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["call_key"] == call_key)
        .collect()
}

#[tokio::test]
async fn s2_withdrawn_held_request_ends_not_started_and_never_runs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let warm = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let marker = root.join("project/withdrawn-marker");
    let body =
        json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"held"});
    let mut held = route.start(body.clone()).await;
    let ask = route.wire.lock().await.next().await.unwrap();
    assert_eq!(ask.header.ty, FrameType::Request);
    // Resending the same keyed call while it waits on its question attaches
    // to that call: no second question, and it ends with the same frame.
    let mut repeat = route.start(body).await;
    let withdrawn = route.terminal(withdraw_body("held")).await;
    assert_eq!(withdrawn.header.ty, FrameType::Response);
    assert_eq!(response_json(&withdrawn), json!({"answer": "withdrawn"}));
    let ended = held.terminal(Duration::from_secs(5)).await;
    assert_eq!(ended.header.ty, FrameType::Error);
    assert_eq!(
        response_json(&ended),
        json!({"code": "not_started", "message": "call was not started",
               "detail": {"call_key": "held", "reason": "withdrawn"}})
    );
    let attached = repeat.terminal(Duration::from_secs(5)).await;
    assert_eq!(
        (attached.header.ty, &attached.body),
        (ended.header.ty, &ended.body)
    );
    // The question was withdrawn: a late "allow" finds nothing to run.
    route
        .wire
        .send(
            &Frame::build(
                FrameType::Response,
                flags(),
                ask.header.channel,
                ask.header.epoch,
                ask.header.corr,
                serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!marker.exists(), "the withdrawn call ran");
    assert_eq!(
        route.terminal(withdraw_body("held")).await.body,
        withdrawn.body
    );
    // A route bound without the tool-provider v1 declaration still refuses
    // tool.withdraw, late_results and late_results.ack by name.
    let legacy = bind_route_declaring(&process, &root.join("project"), "runner", "legacy", None)
        .await
        .unwrap();
    for body in [
        withdraw_body("held"),
        json!({"name": "late_results", "arguments": {"since": null}}),
        json!({"name": "late_results.ack", "arguments": {"through": {"provider_incarnation": "x", "seq": 0}}}),
    ] {
        let refused = legacy.terminal(body).await;
        assert_eq!(refused.header.ty, FrameType::Error);
        assert_eq!(response_json(&refused)["code"], "unsupported_operation");
    }
}

/// A withdraw that settles a held call after its admission committed but
/// before the frame loop filed its question. The admission gate orders it:
/// hold the request between admission and question filing, withdraw, then
/// release. The request must end `not_started` without a question being filed
/// and without its command running.
#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn s2_withdraw_before_the_question_is_filed_ends_the_request_not_started() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let warm = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let gate = root.join("admission-gate");
    let held_signal = root.join("admission-gate.held");
    std::fs::write(&gate, b"hold").unwrap();
    let marker = root.join("project/unfiled-marker");
    let mut held = route
        .start(json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"unfiled"}))
        .await;
    // Admission committed `Prepared`; the request has not reached the frame loop.
    wait_for_file(&held_signal).await;
    let withdrawn = route.terminal(withdraw_body("unfiled")).await;
    assert_eq!(withdrawn.header.ty, FrameType::Response);
    assert_eq!(response_json(&withdrawn), json!({"answer": "withdrawn"}));
    std::fs::remove_file(&gate).unwrap();
    let ended = held.terminal(Duration::from_secs(10)).await;
    assert_eq!(
        ended.header.ty,
        FrameType::Error,
        "{}",
        String::from_utf8_lossy(&ended.body)
    );
    assert_eq!(
        response_json(&ended),
        json!({"code": "not_started", "message": "call was not started",
               "detail": {"call_key": "unfiled", "reason": "withdrawn"}})
    );
    // No permission question was ever filed for it, and it never ran.
    let mut wire = route.wire.lock().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(500), wire.next())
            .await
            .is_err(),
        "a question was filed for the withdrawn call"
    );
    drop(wire);
    assert!(!marker.exists(), "the withdrawn call ran");
    assert_eq!(
        route.terminal(withdraw_body("unfiled")).await.body,
        withdrawn.body
    );
}

#[tokio::test]
async fn s2_withdraw_of_a_running_keyed_bash_kills_only_that_task() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let route = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    route
        .raw(
            json!({"name":"bash","arguments":{"command":"printf warm"}}),
            false,
        )
        .await;
    let started = |name: &str| root.join(format!("project/{name}-started"));
    let call = |name: &str, seconds: u64| json!({"name":"bash","arguments":{"command":format!("{}; sleep {seconds}", marker_command(&started(name)))},"call_key":name});
    let mut withdrawn = route.start(call("withdrawn", 30)).await;
    let mut spared = route.start(call("spared", 6)).await;
    wait_for_file(&started("withdrawn")).await;
    wait_for_file(&started("spared")).await;
    let first = route.terminal(withdraw_body("withdrawn")).await;
    let second = route.terminal(withdraw_body("withdrawn")).await;
    assert_eq!(first.header.ty, FrameType::Response);
    assert_eq!(
        response_json(&first),
        json!({"answer": "already_started", "outcome": "unknown"})
    );
    assert_eq!(first.body, second.body);
    withdrawn.terminal(Duration::from_secs(15)).await;
    assert!(
        spared.rx.try_recv().is_err(),
        "the other running call was interrupted"
    );
    spared.terminal(Duration::from_secs(15)).await;
    assert_eq!(
        route.terminal(withdraw_body("withdrawn")).await.body,
        first.body
    );
    drop(process);
    let conn = rusqlite::Connection::open(ledger_database(&root).await).unwrap();
    let mut tasks = conn
        .prepare("SELECT json_extract(metadata,'$.call_key.key'), status, json_extract(metadata,'$.status_reason') FROM bash_tasks WHERE json_extract(metadata,'$.call_key.key') IN ('withdrawn','spared') ORDER BY 1")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    tasks.sort();
    assert_eq!(
        tasks,
        vec![
            ("spared".into(), "completed".into(), None),
            (
                "withdrawn".into(),
                "killed".into(),
                Some("withdrawn".into())
            ),
        ]
    );
}

#[tokio::test]
async fn s2_settled_scoped_withdraw_is_identical_across_expiry_for_carrier_and_owner() {
    use aft::db::call_ledger as ledger;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    std::fs::write(root.join("project/input.txt"), "content\n").unwrap();
    let carrier_stamp = scoped_stamp("carrier", "owner", "scope");
    let owner_stamp = scoped_stamp("owner", "owner", "scope");
    let carrier = Subject::head()
        .route(&process, &carrier_stamp)
        .await
        .unwrap();
    let read = json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"settled"});
    assert_eq!(
        response_json(&carrier.raw(read, false).await)["isError"],
        false
    );
    let first = carrier.terminal(withdraw_body("settled")).await;
    assert_eq!(first.header.ty, FrameType::Response);
    let answer = response_json(&first);
    assert_eq!(answer["answer"], "completed");
    assert_eq!(answer["result_retained"], true);
    drop(process);
    let conn = rusqlite::Connection::open(ledger_database(&root).await).unwrap();
    conn.execute(
        "UPDATE call_ledger SET settled_at=settled_at-?1",
        [ledger::RETENTION_MS],
    )
    .unwrap();
    drop(conn);
    let process = Subject::head().restart(&root).await.unwrap();
    let carrier = Subject::head()
        .route(&process, &carrier_stamp)
        .await
        .unwrap();
    let owner = Subject::head().route(&process, &owner_stamp).await.unwrap();
    let after = carrier.terminal(withdraw_body("settled")).await;
    let by_owner = owner
        .terminal(json!({"name":"tool.withdraw","arguments":{"call_key":"settled","carrier":"reserved:carrier"}}))
        .await;
    assert_eq!(
        (after.header.ty, &after.body),
        (first.header.ty, &first.body)
    );
    assert_eq!(
        (by_owner.header.ty, &by_owner.body),
        (first.header.ty, &first.body)
    );
    // The call's late entry was never acked, and the forced expiry above
    // turned it into an `expired` entry.
    let page = late_results_page(&owner).await;
    let entries = entries_for(&page, "settled");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "expired");
}

#[tokio::test]
async fn s2_unobserved_scoped_row_answers_already_started_unknown() {
    use aft::db::call_ledger::{self as ledger, Key, ScopeIdentity, State};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let stamp = scoped_stamp("carrier", "owner", "scope");
    let route = Subject::head().route(&process, &stamp).await.unwrap();
    route
        .raw(
            json!({"name":"read","arguments":{"filePath":"missing.txt"},"call_key":"seed"}),
            false,
        )
        .await;
    drop(process);
    // A read admitted and dispatched, whose end AFT never observed before it
    // stopped: recovery settles it "started, end unobserved".
    let conn = rusqlite::Connection::open(ledger_database(&root).await).unwrap();
    let key = Key {
        carrier: "reserved:carrier".into(),
        call_key: "unobserved".into(),
    };
    ledger::admit(
        &conn,
        &key,
        "digest",
        Some(&ScopeIdentity {
            owner: "reserved:owner".into(),
            scope_ref: "scope".into(),
            scope_epoch: 7,
        }),
        State::DispatchStarted,
    )
    .unwrap();
    drop(conn);
    let process = Subject::head().restart(&root).await.unwrap();
    let route = Subject::head().route(&process, &stamp).await.unwrap();
    let first = route.terminal(withdraw_body("unobserved")).await;
    assert_eq!(first.header.ty, FrameType::Response);
    assert_eq!(
        response_json(&first),
        json!({"answer": "already_started", "outcome": "unknown"})
    );
    assert_eq!(
        route.terminal(withdraw_body("unobserved")).await.body,
        first.body
    );
    let owner = Subject::head()
        .route(&process, &scoped_stamp("owner", "owner", "scope"))
        .await
        .unwrap();
    let page = late_results_page(&owner).await;
    let entries = entries_for(&page, "unobserved");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        (
            &entries[0]["kind"],
            &entries[0]["reduced"],
            &entries[0]["outcome"]
        ),
        (&json!("result"), &json!(true), &json!("unknown"))
    );
}

#[tokio::test]
async fn s2_completed_scoped_bash_reaches_only_its_owner_once_and_is_acked_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let carrier = Subject::head()
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let marker = root.join("project/completed-marker");
    let completed = carrier
        .raw(
            json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"completed"}),
            true,
        )
        .await;
    assert_approved_scoped_shell_result(&completed, &marker);
    let owner = Subject::head()
        .route(&process, &scoped_stamp("owner", "owner", "scope"))
        .await
        .unwrap();
    let page = late_results_page(&owner).await;
    let entries = entries_for(&page, "completed");
    assert_eq!(entries.len(), 1, "{page}");
    assert_eq!(entries[0]["kind"], "result");
    assert_eq!(entries[0]["custodian"], "reserved:owner");
    assert_eq!(entries[0]["result"], response_json(&completed));
    // The carrier is not the custodian: it sees no entry.
    assert_eq!(late_results_page(&carrier).await["entries"], json!([]));
    for _ in 0..2 {
        let acked = owner
            .terminal(json!({"name":"late_results.ack","arguments":{"through": page["cursor"]}}))
            .await;
        assert_eq!(acked.header.ty, FrameType::Response);
        assert_eq!(response_json(&acked), json!({}));
        assert!(entries_for(&late_results_page(&owner).await, "completed").is_empty());
    }
}

/// Bind a tool-provider v1 route, the way a consumer that declared
/// `role_versions: {"tool-provider": "v1"}` on route open reaches AFT.
async fn bind_route(
    handle: &Process,
    project: &Path,
    harness: &str,
    session: &str,
) -> Result<Route, HarnessError> {
    bind_route_declaring(
        handle,
        project,
        harness,
        session,
        Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
    )
    .await
}
async fn bind_route_declaring(
    handle: &Process,
    project: &Path,
    harness: &str,
    session: &str,
    role_versions: Option<BTreeMap<String, String>>,
) -> Result<Route, HarnessError> {
    bind_route_stamped(
        handle,
        project,
        harness,
        session,
        role_versions,
        &RouteStamp {
            principal: "direct".into(),
            scope: None,
        },
    )
    .await
}

async fn bind_route_stamped(
    handle: &Process,
    project: &Path,
    harness: &str,
    session: &str,
    role_versions: Option<BTreeMap<String, String>>,
    stamp: &RouteStamp,
) -> Result<Route, HarnessError> {
    bind_route_stamped_with_elicitation(
        handle,
        project,
        harness,
        session,
        role_versions,
        stamp,
        stamp.scope.is_some(),
    )
    .await
}

async fn bind_route_stamped_with_elicitation(
    handle: &Process,
    project: &Path,
    harness: &str,
    session: &str,
    role_versions: Option<BTreeMap<String, String>>,
    stamp: &RouteStamp,
    elicitation: bool,
) -> Result<Route, HarnessError> {
    bind_route_stamped_with_mode(
        handle,
        project,
        harness,
        session,
        role_versions,
        stamp,
        elicitation.then(|| vec!["elicitation".into()]),
        None,
    )
    .await
}

async fn bind_route_stamped_with_mode(
    handle: &Process,
    project: &Path,
    harness: &str,
    session: &str,
    role_versions: Option<BTreeMap<String, String>>,
    stamp: &RouteStamp,
    consumer_capabilities: Option<Vec<String>>,
    mode: Option<cortexkit_role_tool_provider::CallMode>,
) -> Result<Route, HarnessError> {
    let wire = handle.wire.clone();
    let channel = wire.next_channel.fetch_add(1, Ordering::SeqCst);
    let corr = wire.corr();
    let bind = ModuleControlRequest::RouteBind {
        route_channel: channel,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: BindIdentity::new(project, harness, session),
        principal: Some(decode_principal(&stamp.principal)?),
        consumer_capabilities,
        admission_facts: None,
        scope: stamp
            .scope
            .as_ref()
            .map(|scope| {
                Ok(subc_protocol::scope::ScopeStamp::new(
                    decode_principal(&scope.owner)?,
                    scope.scope_ref.clone(),
                    scope.scope_epoch,
                    subc_protocol::scope::ScopeKind::Head,
                )
                .with_owner_authorized(true)
                .with_attributes(
                    subc_protocol::scope::ScopeAttributes::new().with_flow_id(
                        (mode == Some(cortexkit_role_tool_provider::CallMode::Flow))
                            .then(|| "mode-flow".into()),
                    ),
                ))
            })
            .transpose()?,
        role_versions,
        call_mode: mode,
    };
    let mut reply = wire.expect(0, corr);
    wire.send(
        &Frame::build(
            FrameType::Request,
            flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&bind).unwrap(),
        )
        .map_err(harness_error)?,
    )
    .await?;
    let ack = tokio::time::timeout(Duration::from_secs(30), reply.rx.recv())
        .await
        .map_err(harness_error)?
        .ok_or_else(|| HarnessError::new("module EOF"))?;
    if ack.header.ty != FrameType::Response || ack.header.corr != corr {
        return Err(HarnessError::new(format!(
            "unexpected bind frame: {:?} {}",
            ack.header.ty,
            String::from_utf8_lossy(&ack.body)
        )));
    }
    let response: ModuleControlResponse =
        serde_json::from_slice(&ack.body).map_err(harness_error)?;
    if response != (ModuleControlResponse::RouteBindAck {}) {
        return Err(HarnessError::new("no bind acknowledgement"));
    }
    Ok(Route {
        wire,
        channel,
        scoped_preset: stamp.scope.as_ref().map(|_| "head"),
        legacy_diagnostics: None,
    })
}
fn decode_principal(principal: &str) -> Result<Principal, HarnessError> {
    match principal {
        "direct" => Ok(Principal::Direct),
        "unverified" => Ok(Principal::Unverified),
        _ => principal
            .strip_prefix("reserved:")
            .map(|module_id| Principal::Reserved {
                module_id: module_id.into(),
            })
            .ok_or_else(|| HarnessError::new("invalid principal spelling")),
    }
}
impl Route {
    async fn exchange(&self, body: Value, cancel: bool) -> Result<Exchange, RouteFailure> {
        let result = self.exchange_inner(body, cancel).await;
        *self.wire.last_exchange_outcome.lock().unwrap() = Some(match &result {
            Ok(exchange) => format!(
                "terminal_frames={:?}",
                exchange
                    .frames
                    .iter()
                    .filter(|frame| frame.is_terminal())
                    .collect::<Vec<_>>()
            ),
            Err(error) => format!("route_error={error}"),
        });
        result.map_err(|error| RouteFailure::new(error.to_string()))
    }
    async fn exchange_inner(
        &self,
        mut body: Value,
        cancel: bool,
    ) -> Result<Exchange, HarnessError> {
        self.adapt_preset(&mut body);
        let corr = self.wire.corr();
        // Registered before sending, so no reply can arrive unclaimed. Other
        // requests on the same connection proceed concurrently.
        let mut replies = self.wire.expect(self.channel, corr);
        self.wire
            .send(
                &Frame::build(
                    FrameType::Request,
                    flags(),
                    self.channel,
                    1,
                    corr,
                    serde_json::to_vec(&body).unwrap(),
                )
                .map_err(harness_error)?,
            )
            .await?;
        if cancel {
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.wire
                .send(
                    &Frame::build(FrameType::Cancel, flags(), self.channel, 1, corr, vec![])
                        .map_err(harness_error)?,
                )
                .await?;
        }
        let mut exchange = Exchange::default();
        let mut terminal = false;
        loop {
            // Before the terminal frame, wait as long as a held call may stay
            // held; after it, only a short quiet window for a stray second one.
            let wait = if terminal {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(60)
            };
            let frame = match tokio::time::timeout(wait, replies.rx.recv()).await {
                Ok(Some(frame)) => frame,
                Ok(None) => return Err(HarnessError::new("module EOF")),
                Err(_) if terminal => break,
                Err(elapsed) => return Err(harness_error(elapsed)),
            };
            let observed = match frame.header.ty {
                FrameType::Response => ObservedFrame::Response(
                    serde_json::from_slice(&frame.body).map_err(harness_error)?,
                ),
                FrameType::Error => ObservedFrame::Error(
                    serde_json::from_slice(&frame.body).map_err(harness_error)?,
                ),
                FrameType::StreamEnd => ObservedFrame::StreamEnd,
                _ => {
                    ObservedFrame::Data(serde_json::from_slice(&frame.body).map_err(harness_error)?)
                }
            };
            terminal |= observed.is_terminal();
            exchange.frames.push(observed);
        }
        Ok(exchange)
    }

    fn adapt_preset(&self, body: &mut Value) {
        // The published runner omits presets on scoped calls. Adapt only tool
        // calls, not role operations; production admission still requires one.
        if let Some(preset) = self.scoped_preset {
            let name = body["name"].as_str().unwrap_or_default();
            if !matches!(
                name,
                "role.describe"
                    | "tool.catalog"
                    | "tool.withdraw"
                    | "late_results"
                    | "late_results.ack"
            ) && body.get("name").is_some()
            {
                body.as_object_mut()
                    .unwrap()
                    .entry("preset")
                    .or_insert(json!(preset));
            }
        }
    }

    /// Sends `body` and reads the ordered queue of unclaimed frames until its
    /// terminal frame, answering any permission question with `allow` when
    /// `approve` is set and retrying while persistence is still initializing.
    async fn raw(&self, mut body: Value, approve: bool) -> Frame {
        self.adapt_preset(&mut body);
        let mut wire = self.wire.lock().await;
        for _ in 0..100 {
            let corr = wire.corr();
            wire.send(
                &Frame::build(
                    FrameType::Request,
                    flags(),
                    self.channel,
                    1,
                    corr,
                    serde_json::to_vec(&body).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            loop {
                let frame = wire.next().await.unwrap();
                if frame.header.ty == FrameType::Request && approve {
                    wire.send(
                        &Frame::build(
                            FrameType::Response,
                            flags(),
                            self.channel,
                            1,
                            frame.header.corr,
                            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                    continue;
                }
                assert_eq!(frame.header.channel, self.channel);
                assert_eq!(frame.header.corr, corr);
                assert!(matches!(
                    frame.header.ty,
                    FrameType::Response | FrameType::Error | FrameType::StreamEnd
                ));
                if frame.header.ty == FrameType::Response
                    && (response_json(&frame)["structuredContent"]["code"]
                        == "database_initializing"
                        || String::from_utf8_lossy(&frame.body)
                            .contains("Project persistence is still initializing"))
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    break;
                }
                return frame;
            }
        }
        panic!("database initialization never completed");
    }
}
#[async_trait]
impl ToolRoute for Route {
    async fn request(&self, body: Value) -> Result<Exchange, RouteFailure> {
        let key = body["name"]
            .as_str()
            .map(|tool| legacy_probe_key(tool, &body["arguments"]));
        let expected = self.legacy_diagnostics.as_ref().and_then(|pending| {
            key.as_ref()
                .and_then(|key| pending.lock().unwrap().remove(key))
        });
        let exchange = self.exchange(body, false).await?;
        if let Some(expected) = expected {
            for frame in &exchange.frames {
                if let ObservedFrame::Response(actual) = frame {
                    if let Some(diagnostic) = legacy_mismatch_diagnostic(&expected, actual) {
                        eprintln!("legacy reply mismatch: {diagnostic}");
                    }
                }
            }
        }
        Ok(exchange)
    }
    async fn request_then_cancel(&self, body: Value) -> Result<Exchange, RouteFailure> {
        self.exchange(body, true).await
    }
}
#[async_trait]
impl ToolProviderSubject for Subject {
    /// A preset that serves `bash` declares the held-call, late-result,
    /// cancellation and scope cases; `approval_execution` also needs the kill
    /// hook, which only a `test-timing-hooks` build has. The reader serves no
    /// shell tool, so it declares none of them and has no mode probe.
    fn capabilities(&self) -> BTreeSet<Capability> {
        let mut declared = BTreeSet::from([
            Capability::DisableTool,
            Capability::CallKey,
            Capability::SchemaPin,
            Capability::SystemText,
            Capability::LegacyReply,
        ]);
        if self.serves_shell() {
            declared.extend([
                Capability::CallModes,
                Capability::Cancellation,
                Capability::HeldCalls,
                Capability::ScopeStamp,
                Capability::LateResults,
            ]);
            if cfg!(feature = "test-timing-hooks") {
                declared.insert(Capability::ApprovalExecution);
            }
        }
        declared
    }
    fn plain_stamp(&self) -> RouteStamp {
        RouteStamp {
            principal: "direct".into(),
            scope: None,
        }
    }
    /// Three distinct reserved principals outside AFT's first-party list, so
    /// every scoped bind is Untrusted and a keyed shell call files a question.
    fn scoped_principals(&self) -> Option<ScopedPrincipals> {
        let scope = |scope_ref: &str| cortexkit_role_tool_provider::scope::ScopeIdentity {
            owner: "reserved:conformance-owner".into(),
            scope_ref: scope_ref.into(),
            scope_epoch: 7,
        };
        self.serves_shell().then(|| ScopedPrincipals {
            scope: scope("conformance-scope"),
            carrier: "reserved:conformance-carrier".into(),
            other_carrier: "reserved:conformance-other-carrier".into(),
            other_scope: scope("conformance-other-scope"),
        })
    }
    fn catalog_arguments(&self) -> Value {
        match self.preset {
            Some(preset) => json!({ "preset": preset }),
            None => json!({}),
        }
    }
    fn system_text_catalog_arguments(&self) -> Option<Value> {
        let preset = self.preset.unwrap_or("head");
        Some(json!({
            "preset": preset,
            "system_text": {"preset": preset, "params": {}}
        }))
    }
    fn quick_call(&self) -> CallSpec {
        // The quick call's tool must be in the catalog under test, and the
        // reader preset serves no `status`.
        if self.preset == Some("reader") {
            ("glob".into(), json!({"pattern": "*"}))
        } else {
            ("status".into(), json!({}))
        }
    }
    fn tool_result_v1_calls(&self) -> Vec<CallSpec> {
        let tools = self.served_tools.lock().unwrap();
        let tools = tools
            .as_ref()
            .expect("the subject's live catalog was fetched");
        result_probes()
            .into_iter()
            .filter(|(name, _)| tools.contains(name))
            .collect()
    }
    fn legacy_reply(&self, tool: &str, arguments: &Value) -> Option<Value> {
        let fixtures = legacy_fixtures();
        let preset = self.preset.unwrap_or("head");
        let tools = self.served_tools.lock().unwrap();
        let tools = tools
            .as_ref()
            .expect("the subject's live catalog was fetched");
        let rows = legacy_rows_for_catalog(&fixtures, preset, tools);
        let row = legacy_row_for_probe(&rows, preset, tool, arguments);
        let project = self.project.lock().unwrap();
        let project = project.as_ref()?;
        let expected = expand_legacy_project_paths(row["reply"].clone(), project);
        self.legacy_diagnostics
            .lock()
            .unwrap()
            .insert(legacy_probe_key(tool, arguments), expected.clone());
        Some(expected)
    }
    fn call_mode_probe(&self) -> Option<cortexkit_role_tool_provider_conformance::CallModeProbe> {
        if self.preset == Some("reader") {
            return None;
        }
        Some(cortexkit_role_tool_provider_conformance::CallModeProbe {
            call: (
                "bash".into(),
                serde_json::json!({"command":"echo mode-probe"}),
            ),
            unlisted_mode: cortexkit_role_tool_provider::CallMode::Flow,
        })
    }
    async fn route_with_consumer_capabilities(
        &self,
        handle: &Process,
        stamp: &RouteStamp,
        consumer_capabilities: &[String],
    ) -> Result<Route, HarnessError> {
        let mut route = bind_route_stamped_with_mode(
            handle,
            &handle.root.join("project"),
            "runner",
            "conformance-session",
            Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
            stamp,
            Some(consumer_capabilities.to_vec()),
            None,
        )
        .await?;
        route.scoped_preset = self.preset.or(Some("head"));
        wait_for_route_ready(&route, self.quick_call()).await;
        Ok(route)
    }
    async fn route_with_mode(
        &self,
        handle: &Process,
        stamp: &RouteStamp,
        mode: &cortexkit_role_tool_provider::CallMode,
    ) -> Result<Route, HarnessError> {
        let mut stamp = stamp.clone();
        if *mode == cortexkit_role_tool_provider::CallMode::Flow {
            stamp.scope = Some(cortexkit_role_harness::ScopeStamp {
                owner: "direct".into(),
                scope_ref: "mode-flow".into(),
                scope_epoch: 1,
            });
        }
        let mut route = bind_route_stamped_with_mode(
            handle,
            &handle.root.join("project"),
            "runner",
            "conformance-session",
            Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
            &stamp,
            None,
            Some(mode.clone()),
        )
        .await?;
        route.scoped_preset = self.preset.or(Some("head"));
        Ok(route)
    }
    fn slow_call(&self) -> Option<CallSpec> {
        self.serves_shell().then(|| {
            (
                "bash".into(),
                json!({"command": "sleep 5", "wait": true, "timeout": 5000}),
            )
        })
    }
    fn disabled_tool(&self) -> Option<String> {
        Some("outline".into())
    }
    /// A keyed `bash` call that writes `marker`. On the runner's scoped
    /// (Untrusted, elicitation-capable) routes AFT holds it behind a
    /// permission question.
    fn held_call(&self, marker: &Path) -> Option<CallSpec> {
        if !self.serves_shell() {
            return None;
        }
        *self.stand_in.markers.lock().unwrap() = marker.parent().map(Path::to_path_buf);
        Some(("bash".into(), json!({"command": marker_command(marker)})))
    }
    async fn await_held(&self, key: &str) -> Result<(), HarnessError> {
        self.stand_in
            .wait_for(key, Duration::from_secs(30), |_| Some(()))
            .await
    }
    /// Answers `allow` to the permission question filed for the held call
    /// sent with `key`, waiting at most 10 s for it to arrive; a question from
    /// any other call is never answered.
    async fn approve(&self, key: &str) -> Result<(), HarnessError> {
        let (wire, frame) = self
            .stand_in
            .wait_for(key, Duration::from_secs(10), |ask| {
                (!ask.answered).then(|| {
                    ask.answered = true;
                    (
                        ask.wire.clone(),
                        Frame::build(
                            FrameType::Response,
                            flags(),
                            ask.channel,
                            ask.epoch,
                            ask.corr,
                            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
                        ),
                    )
                })
            })
            .await?;
        wire.send(&frame.map_err(harness_error)?).await
    }
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// A fresh provider must admit its very first shell call on a main-thread
/// stack the size Windows gives it, without the test first warming any
/// catalog, validator or code path that admission needs.
///
/// The stack size is set before the child is launched, never raised here and
/// never requested from the linker. On Unix the child's stack limit is lowered
/// to 1 MiB in `pre_exec` (see `spawn`). On Windows the main-thread stack is a
/// fixed reservation in the executable's PE header and cannot be lowered at
/// launch time, so `spawn` asserts that the binary already reserves no more
/// than 1 MiB; the test runs wherever the binary under test meets that bound
/// and says in its failure why it cannot there.
#[tokio::test]
async fn cold_bash_admission_on_one_mib_module_stack() {
    if windows_sized_main_stack().is_none() {
        eprintln!("skipped: this platform cannot give the provider a 1 MiB main-thread stack");
        return;
    }
    let subject = Subject::head().with_one_mib_main_stack();
    let root = tempfile::tempdir().unwrap();
    let process = subject.spawn(root.path()).await.unwrap();
    // Binding a route serves `role.describe`, which touches no catalog. The
    // conformance `route` helper would send a readiness call first, and that
    // call would warm admission before the call under test.
    let route = bind_route_stamped(
        &process,
        &process.root.join("project"),
        "runner",
        "conformance-session",
        Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
        &subject.plain_stamp(),
    )
    .await
    .unwrap();
    // A valid bash call with an option combination translation refuses. It
    // passes admission's schema check and stops before any shell starts, so
    // this reply proves the admission path ran to completion. While the
    // project is still opening its database, the call is answered without
    // being admitted at all, so ask again until admission is the thing that
    // answers.
    let body = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let reply = route
                .start(json!({
                    "op": "tool.call", "preset": "head", "name": "bash",
                    "arguments": {"command": "exit 7", "wait": true, "pty": true}
                }))
                .await
                .terminal(Duration::from_secs(30))
                .await;
            let body = response_json(&reply);
            assert_eq!(
                reply.header.ty,
                FrameType::Response,
                "first bash admission on a 1 MiB main-thread stack: {body}"
            );
            if body["structuredContent"]["code"] != "database_initializing" {
                break body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the project opened its database within 30 seconds");
    assert_eq!(
        body["structuredContent"]["code"], "invalid_request",
        "first bash admission on a 1 MiB main-thread stack: {body}"
    );
    assert_eq!(
        body["structuredContent"]["message"],
        "bash: wait:true cannot be used with pty:true because PTY sessions run in background"
    );
}

/// Runs the conformance suite against the head subject (no preset sent) and
/// checks its outcomes against the inventory pinned for `head` in
/// `fixtures/tool_provider_conformance.json`.
/// Explicit capture only: expected replies never update during verification.
#[tokio::test]
#[ignore = "generate pre-flip legacy baselines from the original main source"]
async fn regenerate_pre_flip_legacy_replies() {
    fn fixed_status_values(value: Value) -> Value {
        match value {
            Value::String(text) if text.starts_with('/') => json!("/conformance"),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(fixed_status_values).collect())
            }
            Value::Object(items) => Value::Object(
                items
                    .into_iter()
                    .map(|(key, value)| (key, fixed_status_values(value)))
                    .collect(),
            ),
            value => value,
        }
    }
    let root = tempfile::tempdir().unwrap();
    let mut bootstrap = Subject::head();
    bootstrap.capture_live_status = true;
    let process = bootstrap.spawn(root.path()).await.unwrap();
    let route = bootstrap
        .route(&process, &bootstrap.plain_stamp())
        .await
        .unwrap();
    let frame = route
        .terminal(json!({"op":"tool.call", "name":"status", "arguments":{}}))
        .await;
    assert_eq!(frame.header.ty, FrameType::Response);
    let body: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(
        body["structuredContent"]["id"], "conformance-result-probe",
        "build the fixture binary with test-conformance-result-inputs"
    );
    let mut snapshot = body["structuredContent"].clone();
    for key in ["id", "success", "text"] {
        snapshot.as_object_mut().unwrap().remove(key);
    }
    let mut fixtures = json!({
        "source_commit":"a209fc15f4ac5866ad3afe5ea111d7ca44ae74e9",
        "source_description":"origin/main at task start, before the catalog flip; only test-only status/response-id input hooks overlaid",
        "renderer_git_blobs": {
            "subc/wire.rs":"323dd1bb64dc9d48fbd5e01bbe04d969ff68d35c",
            "subc_format.rs":"64d5b4b4b7c7d7d86917c8de320d4944bd702df0",
            "run_tool_call.rs":"ce5eb9f1c762183455f3c640ae6338ac82f3343f"
        },
        "binary_sha256":format!("{:x}", Sha256::digest(std::fs::read(aft_binary::aft_binary()).unwrap())),
        "rustc":String::from_utf8(Command::new("rustc").arg("--version").output().unwrap().stdout).unwrap().trim(),
        "profile":"debug, test-conformance-result-inputs",
        "normalization":"only the disposable project root is replaced with {PROJECT}; status snapshot and response id are fixed producer inputs",
        "fixture_project_sha256":format!("{:x}", Sha256::digest(std::fs::read(root.path().join("project/.cortexkit/aft.jsonc")).unwrap())),
        "status_snapshot":fixed_status_values(snapshot),
        "presets":{}
    });
    std::fs::write(
        LEGACY_FIXTURES,
        format!("{}\n", serde_json::to_string_pretty(&fixtures).unwrap()),
    )
    .unwrap();
    drop(route);
    drop(process);
    for preset in ["head", "worker", "reader"] {
        let subject = Subject::new(Some(preset));
        let root = tempfile::tempdir().unwrap();
        let process = subject.spawn(root.path()).await.unwrap();
        let route = subject
            .route(&process, &subject.plain_stamp())
            .await
            .unwrap();
        let catalog = catalog_reply(&route, subject.catalog_arguments()).await;
        assert!(
            catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.get("result_format").is_none()),
            "capture requires pre-flip main's declaration=false"
        );
        let project = root.path().join("project").to_string_lossy().into_owned();
        let mut rows = Vec::new();
        for (name, arguments) in subject.tool_result_v1_calls() {
            let frame = route
                .terminal(
                    json!({"op":"tool.call", "preset":preset, "name":name, "arguments":arguments}),
                )
                .await;
            assert_eq!(
                frame.header.ty,
                FrameType::Response,
                "{preset}/{name}: {}",
                String::from_utf8_lossy(&frame.body)
            );
            let wire_body = String::from_utf8(frame.body)
                .unwrap()
                .replace(&project, "{PROJECT}");
            let reply: Value = serde_json::from_str(&wire_body).unwrap();
            rows.push(
                json!({"tool":name, "arguments":arguments, "reply":reply, "wire_body":wire_body}),
            );
        }
        fixtures["presets"][preset] = json!(rows);
    }
    std::fs::write(
        LEGACY_FIXTURES,
        format!("{}\n", serde_json::to_string_pretty(&fixtures).unwrap()),
    )
    .unwrap();
    println!(
        "Captured pre-flip legacy baselines from {}",
        fixtures["source_commit"]
    );
}

/// Extend the legacy-reply fixture without changing any existing reply.
#[tokio::test]
#[ignore = "capture optional PowerShell replies from the pre-flip binary"]
async fn regenerate_pre_flip_powershell_legacy_replies() {
    legacy_probe_arguments_match_pinned_catalogs();
    let mut fixtures = legacy_fixtures();
    assert_eq!(
        fixtures["source_commit"],
        "a209fc15f4ac5866ad3afe5ea111d7ca44ae74e9"
    );
    let (name, arguments) = result_probes()
        .into_iter()
        .find(|(name, _)| name == "powershell")
        .unwrap();
    for preset in ["head", "worker"] {
        let subject = Subject::new(Some(preset)).with_catalog_powershell();
        let root = tempfile::tempdir().unwrap();
        let process = subject.spawn(root.path()).await.unwrap();
        let route = subject
            .route(&process, &subject.plain_stamp())
            .await
            .unwrap();
        let catalog = catalog_reply(&route, subject.catalog_arguments()).await;
        assert!(
            catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "powershell"),
            "PowerShell must actually be advertised for this capture"
        );
        assert!(
            catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.get("result_format").is_none()),
            "capture requires the pre-flip declaration=false binary"
        );
        let frame = route
            .terminal(
                json!({"op":"tool.call", "preset":preset, "name":name, "arguments":arguments}),
            )
            .await;
        assert_eq!(frame.header.ty, FrameType::Response);
        let wire_body = String::from_utf8(frame.body).unwrap().replace(
            root.path().join("project").to_string_lossy().as_ref(),
            "{PROJECT}",
        );
        let reply: Value = serde_json::from_str(&wire_body).unwrap();
        let captured =
            json!({"tool":name, "arguments":arguments, "reply":reply, "wire_body":wire_body});
        let rows = fixtures["presets"][preset].as_array_mut().unwrap();
        if let Some(existing) = rows.iter().find(|row| row["tool"] == "powershell") {
            assert_eq!(
                existing, &captured,
                "do not overwrite an existing pre-flip oracle"
            );
        } else {
            rows.push(captured);
        }
        subject.assert_catalog_powershell_fixture_policy();
        eprintln!("captured pre-flip {preset}/powershell; platform executable policy verified");
    }
    fixtures["optional_tool_captures"]["powershell"] = json!({
        "source_commit":fixtures["source_commit"].clone(),
        "binary_sha256":format!("{:x}", Sha256::digest(std::fs::read(aft_binary::aft_binary()).unwrap())),
        "catalog_references":["p1_runner_pwsh", "worker_preset_pwsh"],
        "availability":if cfg!(windows) { "real pwsh.exe on Windows; no invocation marker applies" } else { "executable-presence pwsh fixture on PATH; invocation marker remained absent" },
        "capture":"real pre-flip tool.call powershell; production wait:true/pty:true validation refusal"
    });
    std::fs::write(
        LEGACY_FIXTURES,
        format!("{}\n", serde_json::to_string_pretty(&fixtures).unwrap()),
    )
    .unwrap();
}

/// Native error codes and messages are platform content, not path formatting.
#[cfg(windows)]
#[tokio::test]
#[ignore = "capture Windows native errors from the catalog-declaration-off binary"]
async fn regenerate_pre_flip_windows_native_error_replies() {
    let mut fixtures = legacy_fixtures();
    let (name, arguments) = result_probes()
        .into_iter()
        .find(|(name, _)| name == "write")
        .unwrap();
    for preset in ["head", "worker"] {
        let subject = Subject::new(Some(preset));
        let root = tempfile::tempdir().unwrap();
        let process = subject.spawn(root.path()).await.unwrap();
        let route = subject
            .route(&process, &subject.plain_stamp())
            .await
            .unwrap();
        let catalog = catalog_reply(&route, subject.catalog_arguments()).await;
        assert!(
            catalog["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.get("result_format").is_none()),
            "capture requires declaration=false"
        );
        let frame = route
            .terminal(json!({"name":name, "preset":preset, "arguments":arguments}))
            .await;
        assert_eq!(frame.header.ty, FrameType::Response);
        let wire_body = String::from_utf8(frame.body).unwrap();
        let reply: Value = serde_json::from_str(&wire_body).unwrap();
        let captured =
            json!({"tool":name, "arguments":arguments, "reply":reply, "wire_body":wire_body});
        let slot = &mut fixtures["platform_overrides"]["windows"]["presets"][preset];
        if !slot.is_null() {
            assert_eq!(
                slot,
                &json!([captured]),
                "do not refresh an existing native oracle"
            );
        } else {
            *slot = json!([captured]);
        }
    }
    fixtures["platform_overrides"]["windows"]["provenance"] = json!({
        "source_commit":fixtures["source_commit"].clone(),
        "binary_sha256":format!("{:x}", Sha256::digest(std::fs::read(aft_binary::aft_binary()).unwrap())),
        "capture":"native Windows write to an existing directory; original-main renderer with declaration=false and authorized input hooks"
    });
    std::fs::write(
        LEGACY_FIXTURES,
        format!("{}\n", serde_json::to_string_pretty(&fixtures).unwrap()),
    )
    .unwrap();
}

#[test]
fn legacy_probe_arguments_match_pinned_catalogs() {
    let head: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_catalog.json")).unwrap();
    let presets: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_catalog_presets.json")).unwrap();
    let probes = result_probes();
    for preset in ["head", "worker", "reader"] {
        let fixtures = if preset == "head" { &head } else { &presets };
        let fixture = fixtures
            .as_array()
            .unwrap()
            .iter()
            .find(|fixture| {
                fixture["request"]["preset"].as_str().unwrap_or("head") == preset
                    && fixture["powershell_available"] == true
            })
            .unwrap();
        for tool in fixture["reply"]["tools"].as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            let (_, arguments) = probes
                .iter()
                .find(|(probe, _)| probe == name)
                .unwrap_or_else(|| panic!("no probe for {preset}/{name}"));
            assert_eq!(tool["result_format"], "tool-result/v1");
            let validator = jsonschema::validator_for(&tool["input_schema"]).unwrap();
            assert!(
                validator.is_valid(arguments),
                "invalid arguments for pinned {preset}/{name}: {arguments}"
            );
        }
    }
}

#[test]
fn legacy_baselines_cover_optional_powershell_catalog_shapes() {
    let head: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_catalog.json")).unwrap();
    let presets: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_catalog_presets.json")).unwrap();
    for preset in ["head", "worker"] {
        for available in [false, true] {
            let fixtures = if preset == "head" { &head } else { &presets };
            let fixture = fixtures
                .as_array()
                .unwrap()
                .iter()
                .find(|fixture| {
                    fixture["request"]["preset"].as_str().unwrap_or("head") == preset
                        && fixture["powershell_available"] == available
                })
                .unwrap();
            let tools: BTreeSet<String> = fixture["reply"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(tools.contains("powershell"), available);
            let subject = Subject::new(Some(preset));
            *subject.project.lock().unwrap() = Some(PathBuf::from("/fixture/project"));
            *subject.served_tools.lock().unwrap() = Some(tools);
            let probes = subject.tool_result_v1_calls();
            let powershell = probes.iter().find(|(tool, _)| tool == "powershell");
            assert_eq!(powershell.is_some(), available);
            if let Some((tool, arguments)) = powershell {
                assert!(
                    subject.legacy_reply(tool, arguments).is_some(),
                    "{preset}: pwsh-present catalog has no matching pre-flip powershell baseline"
                );
            }
        }
    }
}

#[test]
#[should_panic(
    expected = "live head catalog advertises powershell, but no pre-flip powershell baseline is available"
)]
fn advertised_powershell_without_a_baseline_fails_explicitly() {
    let mut fixtures = legacy_fixtures();
    fixtures["presets"]["head"]
        .as_array_mut()
        .unwrap()
        .retain(|row| row["tool"] != "powershell");
    legacy_rows_for_catalog(
        &fixtures,
        "head",
        &BTreeSet::from(["powershell".to_owned()]),
    );
}

#[tokio::test]
async fn real_legacy_probe_wire_bytes_match_pre_flip_baselines() {
    for preset in ["head", "worker", "reader"] {
        assert_legacy_probe_wire_bytes(Subject::new(Some(preset))).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_oracle_preserves_supplied_symlink_root() {
    let fixture = tempfile::tempdir().unwrap();
    let physical = fixture.path().join("physical-root");
    let supplied = fixture.path().join("supplied-root");
    std::fs::create_dir(&physical).unwrap();
    std::os::unix::fs::symlink(&physical, &supplied).unwrap();
    assert_ne!(
        supplied,
        std::fs::canonicalize(&supplied).unwrap(),
        "the regression must exercise two spellings of the same root"
    );
    assert_legacy_probe_wire_bytes_at(Subject::head(), &supplied).await;
}

async fn assert_legacy_probe_wire_bytes(subject: Subject) {
    let root = tempfile::tempdir().unwrap();
    assert_legacy_probe_wire_bytes_at(subject, root.path()).await;
}

async fn assert_legacy_probe_wire_bytes_at(subject: Subject, root: &Path) {
    let fixtures = legacy_fixtures();
    assert_eq!(
        fixtures["source_commit"],
        "a209fc15f4ac5866ad3afe5ea111d7ca44ae74e9"
    );
    let preset = subject.preset.unwrap_or("head");
    let process = subject.spawn(root).await.unwrap();
    let project = subject.project.lock().unwrap().clone().unwrap();
    assert_eq!(
        project,
        process.root.join("project"),
        "the oracle must use the exact project spelling sent on the route"
    );
    let route = subject
        .route(&process, &subject.plain_stamp())
        .await
        .unwrap();
    let tools = subject.served_tools.lock().unwrap().clone().unwrap();
    let rows = legacy_rows_for_catalog(&fixtures, preset, &tools);
    let probes = subject.tool_result_v1_calls();
    assert_eq!(
        rows.len(),
        probes.len(),
        "every live {preset} probe has a pre-flip baseline"
    );
    for (name, arguments) in probes {
        let expected = legacy_row_for_probe(&rows, preset, &name, &arguments);
        eprintln!("legacy probe {preset}/{name}: sending {arguments}");
        let frame = route
            .terminal(
                json!({"op":"tool.call", "preset":preset, "name":name, "arguments":arguments}),
            )
            .await;
        eprintln!(
            "legacy probe {preset}/{name}: received {:?}",
            frame.header.ty
        );
        assert_eq!(frame.header.ty, FrameType::Response);
        let actual = String::from_utf8(frame.body).unwrap();
        assert_eq!(
            serde_json::to_string(&expected["reply"]).unwrap(),
            expected["wire_body"].as_str().unwrap(),
            "pinned legacy JSON and bytes must describe the same reply"
        );
        let expected_reply = expand_legacy_project_paths(expected["reply"].clone(), &project);
        let expected_wire_body = serde_json::to_string(&expected_reply).unwrap();
        let diagnostic = serde_json::from_str::<Value>(&actual)
            .ok()
            .and_then(|actual| legacy_mismatch_diagnostic(&expected_reply, &actual))
            .unwrap_or_else(|| "JSON values match; serialized bytes differ".to_owned());
        assert_eq!(
            actual, expected_wire_body,
            "pre-flip wire bytes: {preset}/{name}; {diagnostic}"
        );
    }
    eprintln!(
        "legacy wire {preset}: powershell={}, {} exact pre-flip replies",
        tools.contains("powershell"),
        rows.len()
    );
    subject.assert_catalog_powershell_fixture_policy();
}

#[tokio::test]
async fn pwsh_catalog_runs_legacy_wire_and_conformance_checks() {
    if !powershell_catalog_case_available() {
        return;
    }
    for preset in ["head", "worker", "reader"] {
        assert_legacy_probe_wire_bytes(Subject::new(Some(preset)).with_catalog_powershell()).await;
        conformance_inventory(Subject::new(Some(preset)).with_catalog_powershell()).await;
    }
}

#[tokio::test]
async fn slice_a_real_module_conformance_inventory() {
    conformance_inventory(Subject::head()).await;
}

/// The published conformance suite passes for every catalog preset AFT
/// serves (head, worker, reader): each one is a complete catalog, refuses an
/// undefined preset, and admits its own quick call and schema pins. The shell
/// presets also pass every held-call, withdraw and late-results case, and,
/// when built with `test-timing-hooks`, the two crash cases that need its
/// kill hook.
#[tokio::test]
async fn every_catalog_preset_passes_the_conformance_suite() {
    for preset in ["head", "worker", "reader"] {
        eprintln!("preset {preset}:");
        conformance_inventory(Subject::new(Some(preset))).await;
    }
}

/// Runs the published suite against the real module and compares its case
/// outcomes with the inventory pinned in the fixture for this preset and this
/// build: `hooks` when the crash kill hook is compiled in, `no_hooks`
/// otherwise. The runner combines declared capabilities with capabilities
/// inferred from live discovery, including the preset-less catalog's list.
/// The feature-off build skips the cases that need `approval_execution`.
///
/// Known gap in the conformance runner (the
/// `cortexkit-role-tool-provider-conformance` dev-dependency, from 0.5.0 up
/// to the pinned version): it never sets `preset` on a tool call, while AFT
/// refuses a preset-less tool call on a scoped route. The route adapter
/// (`Route::adapt_preset`) therefore adds the subject's preset to every
/// scoped tool call that is not a role operation.
async fn conformance_inventory(subject: Subject) {
    use cortexkit_role_tool_provider_conformance::{CaseOutcome, SuiteVerdict, CASES};
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_conformance.json")).unwrap();
    let build = if cfg!(feature = "test-timing-hooks") {
        "hooks"
    } else {
        "no_hooks"
    };
    let inventory = &fixtures["slice_b"]["presets"][subject.preset.unwrap_or("head")][build];
    let expected =
        |field: &str| serde_json::from_value::<BTreeSet<String>>(inventory[field].clone()).unwrap();
    let (expected_enabled, expected_skipped, expected_inapplicable) = (
        expected("enabled_cases"),
        expected("skipped_cases"),
        expected("inapplicable_cases"),
    );
    // Case IDs are pinned from the conformance crate: the inventory names
    // every case the crate runs, each exactly once.
    let every: BTreeSet<String> = CASES.iter().map(|case| case.name.to_string()).collect();
    assert!(expected_enabled.is_disjoint(&expected_skipped));
    assert!(expected_enabled.is_disjoint(&expected_inapplicable));
    assert!(expected_skipped.is_disjoint(&expected_inapplicable));
    assert_eq!(
        expected_enabled
            .iter()
            .chain(&expected_skipped)
            .chain(&expected_inapplicable)
            .cloned()
            .collect::<BTreeSet<_>>(),
        every
    );
    let declared: BTreeSet<String> = subject
        .capabilities()
        .into_iter()
        .map(|capability| capability.name().to_string())
        .collect();
    assert_eq!(declared, expected("capabilities"));
    let dir = tempfile::tempdir().unwrap();
    let report =
        cortexkit_role_tool_provider_conformance::run_suite(&subject, &dir.path().join("run"))
            .await
            .unwrap();
    subject.assert_catalog_powershell_fixture_policy();
    let mut enabled = BTreeSet::new();
    let mut skipped = BTreeSet::new();
    let mut inapplicable = BTreeSet::new();
    let mut failed = BTreeSet::new();
    for case in &report.cases {
        match &case.outcome {
            CaseOutcome::Skipped { .. } => {
                skipped.insert(case.case.to_string());
            }
            // An optional promise AFT does not make, such as a manifest
            // capability it does not declare.
            CaseOutcome::Inapplicable { .. } => {
                inapplicable.insert(case.case.to_string());
            }
            CaseOutcome::Passed => {
                enabled.insert(case.case.to_string());
            }
            CaseOutcome::Failed { reason } => {
                enabled.insert(case.case.to_string());
                failed.insert(case.case.to_string());
                eprintln!(
                    "FAIL {}: {}",
                    case.case,
                    reason.chars().take(400).collect::<String>()
                );
            }
        }
    }
    assert_eq!(enabled, expected_enabled);
    assert_eq!(skipped, expected_skipped);
    assert_eq!(inapplicable, expected_inapplicable);
    for case in ["tool_result_v1_valid", "tool_result_legacy_route_unchanged"] {
        assert_eq!(report.outcome(case), Some(&CaseOutcome::Passed));
        eprintln!(
            "{} ({build}): {case} PASS",
            subject.preset.unwrap_or("head")
        );
    }
    // Every route is bound with role_versions {"tool-provider": "v1"}, so the
    // real module admits these calls under the v1 grammar. Every enabled case
    // must pass; the provider conforms for the capabilities it declares and
    // the skipped ones remain undeclared.
    assert!(failed.is_empty(), "failed conformance cases: {failed:?}");
    assert_eq!(
        report.outcome("catalog_listed_presets_served"),
        Some(&CaseOutcome::Passed),
        "the live preset list must enable and pass the served-presets case"
    );
    eprintln!(
        "v1 binds ({build}): {} passed, {} failed, {} capability skips, {} not applicable, {} kills",
        enabled.len() - failed.len(),
        failed.len(),
        skipped.len(),
        inapplicable.len(),
        report.kills.len()
    );
    assert!(
        matches!(
            report.verdict,
            SuiteVerdict::ConformingForDeclaredCapabilities { .. }
        ),
        "{:?}",
        report.verdict
    );
    // A selected crash case kills the module: the verdict then rests on at
    // least one kill that ended a real process.
    let crashes = expected_enabled
        .iter()
        .filter(|case| case.starts_with("crash_"))
        .count();
    assert_eq!(report.kills.len(), crashes, "{:?}", report.kills);
    if crashes > 0 {
        assert!(
            report
                .kills
                .iter()
                .any(|kill| kill.mechanism.is_real_process_kill()),
            "{:?}",
            report.kills
        );
    }
}

#[test]
fn registry_manifest_lock_fixture_equalities_and_independent_declaration() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_conformance.json")).unwrap();
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml")).unwrap();
    let lock: toml::Value = toml::from_str(include_str!("../../../Cargo.lock")).unwrap();
    for artifact in fixtures["registry_artifacts"].as_array().unwrap() {
        let name = artifact["name"].as_str().unwrap();
        let section = if artifact["dependency_kind"] == "normal" {
            "dependencies"
        } else {
            "dev-dependencies"
        };
        assert_eq!(
            manifest[section][name].as_str().unwrap(),
            format!("={}", artifact["version"].as_str().unwrap())
        );
        let package = lock["package"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"].as_str() == Some(name))
            .unwrap();
        for field in ["version", "source", "checksum"] {
            assert_eq!(
                package[field].as_str().unwrap(),
                artifact[field].as_str().unwrap()
            );
        }
        assert!(!artifact.as_object().unwrap().contains_key("sha"));
    }
    assert_eq!(
        fixtures["slice_a"]["declaration"]["majors"][0]["ops"],
        json!(["role.describe", "tool.catalog", "tool.call"])
    );
    assert_eq!(
        fixtures["slice_b"]["declaration"]["majors"][0]["ops"],
        json!([
            "role.describe",
            "tool.catalog",
            "tool.call",
            "tool.withdraw",
            "late_results",
            "late_results.ack"
        ])
    );
}

async fn catalog_reply(route: &Route, request: Value) -> Value {
    let exchange = route
        .request(json!({"name":"tool.catalog","arguments":request}))
        .await
        .unwrap();
    match cortexkit_role_tool_provider_conformance::single_terminal(&exchange).unwrap() {
        ObservedFrame::Response(reply) => reply.clone(),
        other => panic!("catalog refused: {other:?}"),
    }
}

#[tokio::test]
async fn real_module_project_harness_matrix_rebind_and_restart_identity() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let process = Subject::head().spawn(&state).await.unwrap();
    std::fs::create_dir_all(state.join("config/cortexkit")).unwrap();
    std::fs::write(state.join("config/cortexkit/aft.jsonc"), serde_json::to_vec(&json!({"disabled_tools": [], "harnesses": {"runner": {"disabled_tools": ["aft_outline"]}, "opencode": {"disabled_tools": ["aft_search"]}}})).unwrap()).unwrap();
    let p1 = state.join("project");
    let p2 = state.join("project2");
    std::fs::create_dir_all(p2.join(".cortexkit")).unwrap();
    let project_config = |disabled: Vec<&str>| {
        serde_json::to_vec(&json!({"disabled_tools": disabled, "indexes": {"callgraph": false, "trigram": false, "semantic": false}})).unwrap()
    };
    std::fs::write(p1.join(".cortexkit/aft.jsonc"), project_config(vec![])).unwrap();
    std::fs::write(
        p2.join(".cortexkit/aft.jsonc"),
        project_config(vec!["aft_inspect"]),
    )
    .unwrap();
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_catalog.json")).unwrap();
    let request = json!({"system_text":{"preset":"broca","params":{"worker":false}}});
    let mut routes = vec![];
    for (root, harness, fixture) in [
        (&p1, "runner", "p1_runner"),
        (&p1, "opencode", "p1_opencode"),
        (&p2, "runner", "p2_runner"),
        (&p2, "opencode", "p2_opencode"),
    ] {
        let route = bind_route(&process, root, harness, fixture).await.unwrap();
        let reply = catalog_reply(&route, request.clone()).await;
        let available = reply["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "powershell");
        let name = format!("{fixture}{}", if available { "_pwsh" } else { "_no_pwsh" });
        let expected = fixtures
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap();
        assert!(
            serde_json::to_vec(&reply).unwrap() == serde_json::to_vec(&expected["reply"]).unwrap(),
            "{name} full bytes differ"
        );
        routes.push((route, reply));
    }
    std::fs::write(
        p1.join(".cortexkit/aft.jsonc"),
        project_config(vec!["aft_zoom"]),
    )
    .unwrap();
    for (route, reply) in &routes {
        assert_eq!(&catalog_reply(route, request.clone()).await, reply);
    }
    for (harness, fixture) in [
        ("runner", "p1_runner_rebound"),
        ("opencode", "p1_opencode_rebound"),
    ] {
        let route = bind_route(&process, &p1, harness, fixture).await.unwrap();
        let reply = catalog_reply(&route, request.clone()).await;
        let available = reply["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "powershell");
        let name = format!("{fixture}{}", if available { "_pwsh" } else { "_no_pwsh" });
        let expected = fixtures
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap();
        assert_eq!(reply, expected["reply"]);
    }
    for (route, reply) in &routes {
        assert_eq!(&catalog_reply(route, request.clone()).await, reply);
    }
    std::fs::write(p1.join(".cortexkit/aft.jsonc"), project_config(vec![])).unwrap();
    let restored = bind_route(&process, &p1, "runner", "restored")
        .await
        .unwrap();
    assert_eq!(catalog_reply(&restored, request.clone()).await, routes[0].1);
    drop(routes);
    drop(restored);
    drop(process);
    let restarted = Subject::head().restart(&state).await.unwrap();
    std::fs::write(p1.join(".cortexkit/aft.jsonc"), project_config(vec![])).unwrap();
    let route = bind_route(&restarted, &p1, "runner", "restart")
        .await
        .unwrap();
    let reply = catalog_reply(&route, request).await;
    let available = reply["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "powershell");
    let expected = fixtures
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == format!("p1_runner{}", if available { "_pwsh" } else { "_no_pwsh" }))
        .unwrap();
    assert_eq!(reply, expected["reply"]);
    assert!(bind_route(&restarted, &p1, "broca", "unsupported-harness")
        .await
        .is_err());
    // A tool-provider version AFT does not serve refuses the bind and names
    // the versions it does serve, instead of falling back to a legacy route.
    let refused = bind_route_declaring(
        &restarted,
        &p1,
        "runner",
        "unsupported-version",
        Some(BTreeMap::from([("tool-provider".into(), "v2".into())])),
    )
    .await
    .err()
    .expect("an unserved tool-provider version refuses the bind")
    .to_string();
    assert!(
        refused.contains("unsupported_role_version") && refused.contains("[\"v1\"]"),
        "{refused}"
    );
}

#[tokio::test]
async fn s1_keyed_authorization_storage_failure_answers_once_and_never_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::head().spawn(&root).await.unwrap();
    let warm = Subject::head()
        .route(&process, &Subject::head().plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"read","arguments":{"path":"input.txt"}}),
        false,
    )
    .await;
    let route = Subject::head()
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    for (index, state) in ["Authorized", "DispatchStarted"].into_iter().enumerate() {
        let marker = root.join(format!("project/authorization-{index}-marker"));
        let call_key = format!("authorization-{index}");
        let mut call = json!({"name":"bash", "arguments":{"command":marker_command(&marker)}, "call_key":call_key});
        route.adapt_preset(&mut call);
        let mut wire = route.wire.lock().await;
        let corr = wire.corr();
        wire.send(
            &Frame::build(
                FrameType::Request,
                flags(),
                route.channel,
                1,
                corr,
                serde_json::to_vec(&call).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ask = wire.next().await.unwrap();
        assert_eq!(ask.header.ty, FrameType::Request);
        assert!(!marker.exists());
        let conn = rusqlite::Connection::open(ledger_database(&root).await).unwrap();
        conn.execute_batch(&format!("CREATE TRIGGER refuse_authorization_{index} BEFORE UPDATE ON call_ledger WHEN NEW.call_key='{call_key}' AND NEW.state='{state}' BEGIN SELECT RAISE(ABORT, 'authorization storage fixture failure'); END;")).unwrap();
        for _ in 0..2 {
            wire.send(
                &Frame::build(
                    FrameType::Response,
                    flags(),
                    route.channel,
                    1,
                    ask.header.corr,
                    br#"{"decision":"allow"}"#.to_vec(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        }
        let reply = tokio::time::timeout(Duration::from_secs(1), wire.next())
            .await
            .expect("durable authorization failure must answer before prompt expiry")
            .unwrap();
        assert_eq!(reply.header.corr, corr);
        assert_eq!(response_json(&reply)["isError"], true);
        assert!(response_json(&reply)["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("could not be durably recorded"));
        assert!(
            !marker.exists(),
            "authorization storage failure must never spawn"
        );
        let executions: usize = conn
            .query_row("SELECT COUNT(*) FROM bash_tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(executions, 0);
        wire.send(
            &Frame::build(
                FrameType::Response,
                flags(),
                route.channel,
                1,
                ask.header.corr,
                br#"{"decision":"allow"}"#.to_vec(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(60), wire.next())
                .await
                .is_err(),
            "late/duplicate allows must not emit another original terminal"
        );
    }
}

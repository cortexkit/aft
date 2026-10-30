#![allow(clippy::collapsible_match)]

use std::io::{self, BufReader, Write};

use aft::lsp::jsonrpc::{Notification, RequestId, ServerMessage};
use aft::lsp::transport::{read_message, write_notification};
use serde_json::{json, Value};

fn write_json_message(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let json = serde_json::to_string(value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    write!(writer, "Content-Length: {}\r\n\r\n", json.len())?;
    writer.write_all(json.as_bytes())?;
    writer.flush()
}

fn write_response(writer: &mut impl Write, id: RequestId, result: Value) -> io::Result<()> {
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )
}

fn write_request(writer: &mut impl Write, id: i64, method: &str, params: Value) -> io::Result<()> {
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }),
    )
}

fn record_pull_event(event: &str) -> io::Result<()> {
    if let Some(path) = std::env::var_os("AFT_FAKE_LSP_DYNAMIC_PULL_LOG") {
        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(log, "{event}")?;
    }
    Ok(())
}

fn request_position(params: &Option<Value>) -> (u64, u64) {
    let line = params
        .as_ref()
        .and_then(|value| value.get("position"))
        .and_then(|value| value.get("line"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let character = params
        .as_ref()
        .and_then(|value| value.get("position"))
        .and_then(|value| value.get("character"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    (line, character)
}

fn request_document_uri(params: &Option<Value>) -> Value {
    params
        .as_ref()
        .and_then(|value| value.get("textDocument"))
        .and_then(|value| value.get("uri"))
        .cloned()
        .unwrap_or_else(|| Value::String("file:///unknown".to_string()))
}

fn request_include_declaration(params: &Option<Value>) -> bool {
    params
        .as_ref()
        .and_then(|value| value.get("context"))
        .and_then(|value| value.get("includeDeclaration"))
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

fn request_new_name(params: &Option<Value>) -> String {
    params
        .as_ref()
        .and_then(|value| value.get("newName"))
        .and_then(|value| value.as_str())
        .unwrap_or("renamed")
        .to_string()
}

fn request_document_uri_string(params: &Option<Value>) -> String {
    params
        .as_ref()
        .and_then(|value| value.get("textDocument"))
        .and_then(|value| value.get("uri"))
        .and_then(|value| value.as_str())
        .unwrap_or("file:///unknown")
        .to_string()
}

fn document_uri(params: &Option<Value>) -> Value {
    params
        .as_ref()
        .and_then(|value| value.get("textDocument"))
        .and_then(|value| value.get("uri"))
        .cloned()
        .unwrap_or_else(|| Value::String("file:///unknown".to_string()))
}

fn document_version(params: &Option<Value>) -> Value {
    params
        .as_ref()
        .and_then(|value| value.get("textDocument"))
        .and_then(|value| value.get("version"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn write_custom_notification(
    writer: &mut impl Write,
    method: &str,
    uri: Value,
    version: Value,
) -> io::Result<()> {
    write_notification(
        writer,
        &Notification::new(
            method,
            Some(json!({
                "uri": uri,
                "version": version,
            })),
        ),
    )
}

fn write_publish_diagnostics(
    writer: &mut impl Write,
    uri: Value,
    diagnostics: Value,
) -> io::Result<()> {
    write_publish_diagnostics_versioned(writer, uri, diagnostics, Value::Null)
}

/// Same as `write_publish_diagnostics` but includes the LSP `version` field
/// so the v0.17.3 version-match freshness path can be tested.
///
/// When the env var `AFT_FAKE_LSP_STALE_VERSION` is set, the fake server
/// publishes `version - 1` instead of the actual version, simulating an
/// out-of-order publish that should be rejected as stale by the post-edit
/// freshness check.
fn write_publish_diagnostics_versioned(
    writer: &mut impl Write,
    uri: Value,
    diagnostics: Value,
    version: Value,
) -> io::Result<()> {
    let effective_version = if std::env::var("AFT_FAKE_LSP_STALE_VERSION").is_ok() {
        match &version {
            Value::Number(n) if n.is_i64() => Value::Number((n.as_i64().unwrap() - 1).into()),
            _ => version.clone(),
        }
    } else {
        version
    };

    let mut params = json!({
        "uri": uri,
        "diagnostics": diagnostics,
    });
    if !effective_version.is_null() {
        params["version"] = effective_version;
    }
    write_notification(
        writer,
        &Notification::new("textDocument/publishDiagnostics", Some(params)),
    )
}

fn opened_diagnostics() -> Value {
    let server_status_mode = std::env::var("AFT_FAKE_LSP_SERVER_STATUS").ok();
    if server_status_mode.as_deref() == Some("empty_then_quiescent") {
        return json!([]);
    }
    let warming = matches!(
        server_status_mode.as_deref(),
        Some("1" | "publish_then_quiescent")
    );
    let error_message = if warming {
        "warming garbage diagnostic"
    } else {
        "test diagnostic error"
    };
    json!([
        {
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 5 }
            },
            "severity": 1,
            "code": "E0001",
            "source": "fake-lsp",
            "message": error_message
        },
        {
            "range": {
                "start": { "line": 1, "character": 4 },
                "end": { "line": 1, "character": 10 }
            },
            "severity": 2,
            "source": "fake-lsp",
            "message": "test diagnostic warning"
        }
    ])
}

fn changed_diagnostics() -> Value {
    json!([
        {
            "range": {
                "start": { "line": 2, "character": 1 },
                "end": { "line": 2, "character": 8 }
            },
            "severity": 1,
            "code": "E0002",
            "source": "fake-lsp",
            "message": "test diagnostic after change"
        }
    ])
}

fn write_flycheck_progress(writer: &mut impl Write, kind: &str) -> io::Result<()> {
    let mut value = json!({ "kind": kind });
    if kind == "begin" {
        value["title"] = json!("cargo check");
    } else if kind == "end" {
        if let Ok(message) = std::env::var("AFT_FAKE_LSP_CHECK_END_MESSAGE") {
            value["message"] = json!(message);
        }
    }
    write_notification(
        writer,
        &Notification::new(
            "$/progress",
            Some(json!({ "token": "rust-analyzer/flycheck/0", "value": value })),
        ),
    )
}

fn fake_compile_error() -> Value {
    json!({
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 4 }
        },
        "severity": 1,
        "code": "E0425",
        "source": "rustc",
        "message": "fake compile error"
    })
}

/// Add the `file://` URI of every `.rs` file under `dir`, skipping hidden
/// directories and `target`.
fn collect_rust_file_uris(dir: &std::path::Path, uris: &mut std::collections::BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !name.starts_with('.') && name != "target" {
                collect_rust_file_uris(&path, uris);
            }
        } else if name.ends_with(".rs") {
            if let Ok(uri) = url::Url::from_file_path(&path) {
                uris.insert(uri.to_string());
            }
        }
    }
}

fn push_diagnostics_enabled() -> bool {
    std::env::var("AFT_FAKE_LSP_DISABLE_PUSH").ok().as_deref() != Some("1")
}

/// With AFT_FAKE_LSP_SERVER_STATUS=cargo_lock the fake behaves like
/// rust-analyzer run with `--locked`: when it loads the workspace (at
/// `initialized` and on `rust-analyzer/reloadWorkspace`) it reads the root's
/// Cargo.lock and, if the file contains the word `stale`, reports the locked
/// `cargo metadata` failure; otherwise it reports a healthy, quiescent
/// workspace. Like rust-analyzer, it never re-reads the lockfile on its own.
fn cargo_lock_status_mode() -> bool {
    std::env::var("AFT_FAKE_LSP_SERVER_STATUS").ok().as_deref() == Some("cargo_lock")
}

fn cargo_lock_is_stale(root: Option<&std::path::Path>) -> bool {
    root.and_then(|root| std::fs::read_to_string(root.join("Cargo.lock")).ok())
        .is_some_and(|lock| lock.contains("stale"))
}

/// Report the result of the last workspace load. Like rust-analyzer, a status
/// sent while a new load is running (`quiescent: false`) still carries the
/// previous load's health and message.
fn write_cargo_lock_status(
    writer: &mut impl Write,
    stale: bool,
    quiescent: bool,
) -> io::Result<()> {
    let status = if stale {
        json!({
            "health": "warning",
            "quiescent": quiescent,
            "message": "Failed to read Cargo metadata with dependencies: `cargo metadata` exited with an error: error: cannot update the lock file Cargo.lock because --locked was passed to prevent this",
        })
    } else {
        json!({
            "health": "ok",
            "quiescent": quiescent,
            "message": "workspace analysis is ready",
        })
    };
    write_notification(
        writer,
        &Notification::new("experimental/serverStatus", Some(status)),
    )
}

fn delay_changed_diagnostics_if_requested() {
    if let Some(signal_path) = std::env::var_os("AFT_FAKE_LSP_CHANGE_DELAY_SIGNAL") {
        let _ = std::fs::write(signal_path, b"waiting");
    }
    if let Some(delay_ms) = std::env::var("AFT_FAKE_LSP_CHANGE_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
}

/// Read one `Content-Length` framed message from `reader`, returning the
/// whole frame (headers included) and its body. `None` at end of input.
fn read_raw_frame(reader: &mut impl io::BufRead) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut frame = Vec::new();
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        frame.extend_from_slice(line.as_bytes());
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let length =
        length.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame without length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    frame.extend_from_slice(&body);
    Ok(Some((frame, body)))
}

/// AFT_FAKE_LSP_PROXY=<program>: run the real server `<program>` (with this
/// process's arguments) and relay its messages unchanged, except that with
/// AFT_FAKE_LSP_PROXY_CHECK_DELAY_MS=<ms> the first `cargo check` run it
/// announces reaches the client <ms> late. From that announcement until the
/// delay is over, the run's progress and every published diagnostics report
/// are held back (in order); responses and other notifications pass at once.
/// To the client this is a rust-analyzer that is quiescent and answering
/// requests but has not started its check yet, as a server starved of CPU
/// looks: the check announcement comes seconds after quiescence.
fn run_proxy(program: std::ffi::OsString) -> io::Result<()> {
    use std::sync::{Arc, Mutex};
    let delay = std::env::var("AFT_FAKE_LSP_PROXY_CHECK_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(std::time::Duration::from_millis);
    let mut child = std::process::Command::new(program)
        .args(std::env::args_os().skip(1))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut child_stdin = child.stdin.take().expect("piped stdin");
    let child_stdout = child.stdout.take().expect("piped stdout");
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut child_stdin);
    });

    struct Hold {
        /// Frames held back while the delay runs.
        queue: Vec<Vec<u8>>,
        /// Whether frames of the held kind are being held now.
        active: bool,
        /// Whether the delay has already been applied once.
        used: bool,
    }
    let hold = Arc::new(Mutex::new(Hold {
        queue: Vec::new(),
        active: false,
        used: false,
    }));
    let write_frame = |frame: &[u8]| -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(frame)?;
        stdout.flush()
    };
    let mut reader = BufReader::new(child_stdout);
    while let Some((frame, body)) = read_raw_frame(&mut reader)? {
        let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let method = message.get("method").and_then(Value::as_str);
        let check_progress = method == Some("$/progress")
            && message
                .pointer("/params/token")
                .and_then(Value::as_str)
                .is_some_and(|token| token.contains("flycheck"));
        let held_kind = check_progress || method == Some("textDocument/publishDiagnostics");
        let mut state = hold.lock().expect("hold lock");
        if let Some(delay) = delay {
            let begins_check = check_progress
                && message
                    .pointer("/params/value/kind")
                    .and_then(Value::as_str)
                    == Some("begin");
            if begins_check && !state.used {
                state.used = true;
                state.active = true;
                let hold = Arc::clone(&hold);
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    let mut state = hold.lock().expect("hold lock");
                    let mut stdout = io::stdout().lock();
                    for frame in state.queue.drain(..) {
                        let _ = stdout.write_all(&frame);
                    }
                    let _ = stdout.flush();
                    state.active = false;
                });
            }
        }
        if held_kind && state.active {
            state.queue.push(frame);
        } else {
            write_frame(&frame)?;
        }
    }
    let status = child.wait()?;
    std::process::exit(status.code().unwrap_or(1));
}

pub(crate) fn main() -> io::Result<()> {
    if std::env::var_os("AFT_FAKE_CARGO_CHECK").is_some() {
        match std::env::args().nth(1).as_deref() {
            Some("check") => {
                if let Some(marker) = std::env::var_os("AFT_FAKE_CARGO_CHECK_MARKER") {
                    std::fs::write(marker, "checked\n")?;
                }
                println!("{{\"reason\":\"build-finished\",\"success\":true}}");
                return Ok(());
            }
            Some("metadata") => {
                let target = std::env::current_dir()?.join("target");
                println!("{}", serde_json::json!({"target_directory": target}));
                return Ok(());
            }
            Some("-vV") => {
                println!("aft fake rustc 1");
                return Ok(());
            }
            _ => {}
        }
    }
    if let Some(program) = std::env::var_os("AFT_FAKE_LSP_PROXY") {
        // Version probes are plain text, not JSON-RPC frames. Forward them
        // directly so persistence tests fingerprint the real analyzer version.
        if std::env::args_os().any(|arg| arg == "--version") {
            let status = std::process::Command::new(program)
                .args(std::env::args_os().skip(1))
                .status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        return run_proxy(program);
    }
    if std::env::args_os().any(|arg| arg == "--version") {
        println!("aft fake rust-analyzer 1");
        return Ok(());
    }
    // Change cwd before handling initialize, so its response confirms that
    // deletion tests can remove the workspace even on Windows.
    if let Some(cwd) = std::env::var_os("AFT_FAKE_LSP_CWD") {
        std::env::set_current_dir(cwd)?;
    }
    // AFT_FAKE_LSP_IGNORE_SIGTERM=1: keep running through SIGTERM, like a
    // server with its own handler that is busy or wedged, so only a kill that
    // cannot be ignored stops it.
    #[cfg(unix)]
    if std::env::var("AFT_FAKE_LSP_IGNORE_SIGTERM").ok().as_deref() == Some("1") {
        // SAFETY: installing SIG_IGN runs no handler code.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    // AFT_FAKE_LSP_START_DELAY_MS=<ms>: wait before doing anything, like a
    // server process that is slow to come up on a loaded machine. Nothing,
    // not even the pid file below, exists until the delay has passed.
    if let Some(delay_ms) = std::env::var("AFT_FAKE_LSP_START_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
    if let Some(pid_dir) = std::env::var_os("AFT_FAKE_LSP_PID_DIR") {
        std::fs::write(
            std::path::Path::new(&pid_dir).join(std::process::id().to_string()),
            b"started",
        )?;
    }
    if let Some(signal_path) = std::env::var_os("AFT_FAKE_LSP_STARTED_SIGNAL") {
        std::fs::write(signal_path, b"started")?;
    }
    if let Some(path) = std::env::var_os("AFT_FAKE_LSP_GIT_OPTIONAL_LOCKS_FILE") {
        let value = std::env::var("GIT_OPTIONAL_LOCKS").unwrap_or_else(|_| "<unset>".into());
        std::fs::write(path, value)?;
    }
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    // Messages are read on their own thread so the main loop can also act on
    // a timer: a cargo_lock-mode workspace reload finishes a while after it
    // was requested, and other requests (a `didOpen`) are answered meanwhile,
    // as rust-analyzer answers them during a load.
    let (message_tx, message_rx) = std::sync::mpsc::channel::<io::Result<Option<ServerMessage>>>();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let freeze_release = std::env::var_os("AFT_FAKE_LSP_FREEZE_AFTER_OPEN");
        loop {
            let message = read_message(&mut reader);
            let freeze = freeze_release.is_some()
                && matches!(&message,
                Ok(Some(ServerMessage::Notification { method, .. })) if method == "textDocument/didOpen");
            let last = !matches!(message, Ok(Some(_)));
            if message_tx.send(message).is_err() || last {
                break;
            }
            // Pausing only the analysis loop would still let this thread drain
            // stdin, hiding a full-pipe write behind an unbounded message queue.
            if freeze {
                while !std::path::Path::new(freeze_release.as_ref().unwrap()).exists() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    });
    // In cargo_lock mode, the time at which the workspace reload in progress
    // finishes and its final status is sent; `None` when no reload runs.
    let mut reload_finishes_at: Option<std::time::Instant> = None;
    let mut should_register_watched_files = false;
    // The workspace root from `initialize`, for modes that read project files.
    let mut workspace_root: Option<std::path::PathBuf> = None;
    // Whether the last cargo_lock-mode workspace load saw a stale lockfile.
    let mut last_load_stale = false;
    // Emulates rust-analyzer's `cargo check` ("flycheck"): with
    // AFT_FAKE_LSP_FLYCHECK=<ms> a check run begins at `initialized` and, on
    // the first opened document, ends <ms> later after publishing the opened
    // diagnostics plus one compiler error. With AFT_FAKE_LSP_FLYCHECK=never
    // the run begins and never ends.
    let flycheck_mode = std::env::var("AFT_FAKE_LSP_FLYCHECK").ok();
    let mut flycheck_running = false;
    // A cancelled native-diagnostics pull followed by a compiler-only push,
    // as rust-analyzer can send while applying a watched-file change.
    let pull_cancel_mode = std::env::var("AFT_FAKE_LSP_PULL_CANCEL").ok();
    let mut pull_cancelled_once = false;
    // Emulates rust-analyzer's check on save: with
    // AFT_FAKE_LSP_CHECK_ON_SAVE=<ms> the fake asks for save notifications
    // and runs a check <ms> after each trigger: becoming quiescent at
    // `initialized` ("load"), a `didSave` ("save"), and
    // `rust-analyzer/runFlycheck` ("run"). AFT_FAKE_LSP_CHECK_DROP lists the
    // triggers (comma separated) that start no check, as a server that drops
    // one. A check begins, publishes for every open document its opened
    // diagnostics plus, when the file on disk contains `fake_compile_error`,
    // one compiler error, and ends.
    let check_on_save_delay = std::env::var("AFT_FAKE_LSP_CHECK_ON_SAVE")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(std::time::Duration::from_millis);
    let dropped_checks = std::env::var("AFT_FAKE_LSP_CHECK_DROP").unwrap_or_default();
    let schedule_check = |trigger: &str| -> Option<std::time::Instant> {
        let delay = check_on_save_delay?;
        (!dropped_checks
            .split(',')
            .any(|dropped| dropped.trim() == trigger))
        .then(|| std::time::Instant::now() + delay)
    };
    let mut check_begins_at: Option<std::time::Instant> = None;
    let mut check_completed = false;
    // Open documents with their latest version and the diagnostics the fake
    // last published for them from its own analysis, for the emulated check.
    let mut analysis_opens = 0usize;
    let dynamic_pull = std::env::var_os("AFT_FAKE_LSP_DYNAMIC_PULL").is_some();
    let mut client_dynamic_pull = false;
    let mut client_disables_pull = false;
    let mut diagnostic_registered = false;
    let mut open_documents: std::collections::BTreeMap<String, (Value, Value)> =
        std::collections::BTreeMap::new();
    // The text the client last synced for each open document, so
    // AFT_FAKE_LSP_PULL_FROM_CONTENT=1 can answer a pull from what the server
    // was actually told, as a real language server does.
    let mut document_texts: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    // Files the last emulated check found a compiler error in. Like
    // rust-analyzer, every later report for such a file still carries the
    // error until a new check runs.
    let mut check_errors: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let ignore_shutdown = std::env::var("AFT_FAKE_LSP_IGNORE_SHUTDOWN")
        .ok()
        .as_deref()
        == Some("1");

    loop {
        let next_timer = [reload_finishes_at, check_begins_at]
            .into_iter()
            .flatten()
            .min();
        let received = match next_timer {
            Some(fires_at) => match message_rx
                .recv_timeout(fires_at.saturating_duration_since(std::time::Instant::now()))
            {
                Ok(received) => received,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let now = std::time::Instant::now();
                    if reload_finishes_at.is_some_and(|at| at <= now) {
                        reload_finishes_at = None;
                        last_load_stale = cargo_lock_is_stale(workspace_root.as_deref());
                        write_cargo_lock_status(&mut writer, last_load_stale, true)?;
                    }
                    if check_begins_at.is_some_and(|at| at <= now) {
                        check_begins_at = None;
                        write_flycheck_progress(&mut writer, "begin")?;
                        // Like `cargo check`, read every Rust file from disk
                        // and publish for each file it reported on before
                        // or reports on now, and for every open document.
                        let mut files = std::collections::BTreeSet::new();
                        if let Some(root) = &workspace_root {
                            collect_rust_file_uris(root, &mut files);
                        }
                        files.extend(open_documents.keys().cloned());
                        files.extend(check_errors.iter().cloned());
                        let previous_errors = std::mem::take(&mut check_errors);
                        check_errors = files
                            .iter()
                            .filter(|uri| {
                                url::Url::parse(uri)
                                    .ok()
                                    .and_then(|uri| uri.to_file_path().ok())
                                    .and_then(|path| std::fs::read_to_string(path).ok())
                                    .is_some_and(|text| text.contains("fake_compile_error"))
                            })
                            .cloned()
                            .collect();
                        for uri in &files {
                            let open = open_documents.get(uri);
                            if open.is_none()
                                && !previous_errors.contains(uri)
                                && !check_errors.contains(uri)
                            {
                                continue;
                            }
                            let (mut checked, version) =
                                open.cloned().unwrap_or_else(|| (json!([]), Value::Null));
                            if check_errors.contains(uri) {
                                checked
                                    .as_array_mut()
                                    .expect("diagnostics are an array")
                                    .push(fake_compile_error());
                            }
                            write_publish_diagnostics_versioned(
                                &mut writer,
                                Value::String(uri.clone()),
                                checked,
                                version,
                            )?;
                        }
                        write_flycheck_progress(&mut writer, "end")?;
                        check_completed = true;
                        if let Some(path) = std::env::var_os("AFT_FAKE_LSP_CHECK_COMPLETED_SIGNAL") {
                            std::fs::write(path, b"completed")?;
                        }
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match message_rx.recv() {
                Ok(received) => received,
                Err(_) => break,
            },
        };
        let Some(message) = received? else {
            break;
        };
        match message {
            ServerMessage::Request { id, method, params } => match method.as_str() {
                "initialize" => {
                    if let Some(path) = std::env::var_os("AFT_FAKE_LSP_INITIALIZATION_LOG") {
                        use std::io::Write;
                        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
                        writeln!(log, "{}", params.as_ref().unwrap()["initializationOptions"]).unwrap();
                    }
                    if std::env::var("AFT_FAKE_LSP_INIT_NO_REPLY").ok().as_deref() == Some("1")
                        || std::env::var_os("AFT_FAKE_LSP_INIT_NO_REPLY_ONCE").is_some_and(|path| {
                            std::fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(path)
                                .is_ok()
                        })
                    {
                        continue;
                    }
                    if let Some(signal_path) = std::env::var_os("AFT_FAKE_LSP_INIT_DELAY_SIGNAL") {
                        let _ = std::fs::write(signal_path, b"waiting");
                    }
                    // Hold the handshake until the test has observed its caller's
                    // deadline. A release file avoids racing a fixed init delay.
                    if let Some(path) = std::env::var_os("AFT_FAKE_LSP_INIT_RELEASE_FILE") {
                        while !std::path::Path::new(&path).exists() {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                    if let Some(delay_ms) = std::env::var("AFT_FAKE_LSP_INIT_DELAY_MS")
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                    if let Some(target_root_uri) =
                        std::env::var("AFT_FAKE_LSP_INIT_CRASH_ROOT_URI").ok()
                    {
                        let request_root_uri = params
                            .as_ref()
                            .and_then(|value| value.get("rootUri"))
                            .and_then(Value::as_str);
                        if request_root_uri == Some(target_root_uri.as_str()) {
                            eprintln!("intentional initialize failure for {target_root_uri}");
                            std::process::exit(1);
                        }
                    }
                    // Die during initialize the way a broken real server does:
                    // print a diagnostic to stderr, then either exit with the
                    // code in AFT_FAKE_LSP_INIT_EXIT_CODE or be killed by the
                    // signal named in AFT_FAKE_LSP_INIT_SELF_SIGNAL (for
                    // example "TERM"), delivered by the system `kill` utility.
                    let init_exit_code = std::env::var("AFT_FAKE_LSP_INIT_EXIT_CODE")
                        .ok()
                        .and_then(|value| value.parse::<i32>().ok());
                    let init_self_signal = std::env::var("AFT_FAKE_LSP_INIT_SELF_SIGNAL").ok();
                    if init_exit_code.is_some() || init_self_signal.is_some() {
                        if let Ok(text) = std::env::var("AFT_FAKE_LSP_INIT_EXIT_STDERR") {
                            let mut stderr = io::stderr().lock();
                            for line in text.split('|') {
                                let _ = writeln!(stderr, "{line}");
                            }
                            let _ = stderr.flush();
                        }
                        if let Some(signal) = init_self_signal {
                            let _ = std::process::Command::new("kill")
                                .arg(format!("-{signal}"))
                                .arg(std::process::id().to_string())
                                .status();
                            // The signal normally lands before this sleep ends;
                            // the fallback exit keeps a misconfigured test from
                            // hanging.
                            std::thread::sleep(std::time::Duration::from_secs(5));
                            std::process::exit(99);
                        }
                        std::process::exit(init_exit_code.unwrap_or(1));
                    }
                    if std::env::var("AFT_FAKE_LSP_INIT_CRASH_MODULE_NOT_FOUND")
                        .ok()
                        .as_deref()
                        == Some("1")
                    {
                        eprintln!(
                            "Error: Cannot find module '/missing/typescript-language-server/lib/cli.mjs'"
                        );
                        eprintln!("code: 'MODULE_NOT_FOUND'");
                        std::process::exit(1);
                    }
                    if let Some(bytes) = std::env::var("AFT_FAKE_LSP_INIT_STDERR_BYTES")
                        .ok()
                        .and_then(|value| value.parse::<usize>().ok())
                    {
                        let mut stderr = io::stderr().lock();
                        for index in 0..bytes / 80 {
                            let _ = writeln!(
                                stderr,
                                "stderr-fill-line-{index:06}: MODULE_NOT_FOUND cannot find module padding"
                            );
                        }
                        let _ = stderr.flush();
                        std::process::exit(1);
                    }
                    // Capability variants controlled by env vars so tests
                    // can exercise different code paths:
                    //   AFT_FAKE_LSP_PULL=1                → declare diagnosticProvider
                    //   AFT_FAKE_LSP_WORKSPACE=1           → also support workspace/diagnostic
                    //   AFT_FAKE_LSP_NO_WATCHED_FILES=1    → OMIT workspace.didChangeWatchedFiles
                    //                                        (exercises the F5 capability-gate skip path)
                    //   AFT_FAKE_LSP_IGNORE_SHUTDOWN=1      → never reply to shutdown
                    //   (no env)                           → push-only with watched-files support
                    let pull_enabled =
                        std::env::var("AFT_FAKE_LSP_PULL").ok().as_deref() == Some("1");
                    client_disables_pull = params.as_ref().and_then(|p| p.pointer("/initializationOptions/disablePullDiagnostics")).and_then(Value::as_bool) == Some(true);
                    client_dynamic_pull = !client_disables_pull && params.as_ref().and_then(|p| p.pointer("/capabilities/textDocument/diagnostic/dynamicRegistration")).and_then(Value::as_bool) == Some(true);
                    if dynamic_pull { record_pull_event(&format!("initialize dynamic={client_dynamic_pull} disabled={client_disables_pull}"))?; }
                    workspace_root = params
                        .as_ref()
                        .and_then(|value| value.get("rootUri"))
                        .and_then(Value::as_str)
                        .and_then(|uri| url::Url::parse(uri).ok())
                        .and_then(|uri| uri.to_file_path().ok());
                    let workspace_pull =
                        std::env::var("AFT_FAKE_LSP_WORKSPACE").ok().as_deref() == Some("1");
                    let no_watched_files = std::env::var("AFT_FAKE_LSP_NO_WATCHED_FILES")
                        .ok()
                        .as_deref()
                        == Some("1");

                    let mut capabilities = json!({
                        "textDocumentSync": 1,
                        "hoverProvider": true,
                        "definitionProvider": true,
                        "referencesProvider": true,
                        "renameProvider": {
                            "prepareProvider": true
                        }
                    });
                    if check_on_save_delay.is_some() {
                        // rust-analyzer asks for saves without their text.
                        capabilities["textDocumentSync"] = json!({
                            "openClose": true,
                            "change": 1,
                            "save": { "includeText": false }
                        });
                    }

                    if !no_watched_files {
                        // Echo the client capability to exercise defensive
                        // parsing; the registration below is the subscription.
                        capabilities["workspace"] = json!({
                            "didChangeWatchedFiles": {
                                "dynamicRegistration": true
                            }
                        });
                        // AFT_FAKE_LSP_STATIC_WATCHED_FILES_ONLY=1 still
                        // advertises `workspace.didChangeWatchedFiles` in the
                        // initialize result but never sends
                        // client/registerCapability. The echoed field alone
                        // must not enable watched-file notifications.
                        should_register_watched_files =
                            std::env::var("AFT_FAKE_LSP_STATIC_WATCHED_FILES_ONLY")
                                .ok()
                                .as_deref()
                                != Some("1");
                    }

                    if pull_enabled {
                        capabilities["diagnosticProvider"] = json!({
                            "interFileDependencies": true,
                            "workspaceDiagnostics": workspace_pull,
                            "identifier": "fake-lsp"
                        });
                    }

                    write_response(
                        &mut writer,
                        id,
                        json!({
                            "capabilities": capabilities,
                            "serverInfo": {
                                // AFT_FAKE_LSP_SERVER_NAME=<name>: report another
                                // server name, such as the native TypeScript
                                // server's `typescript-go`.
                                "name": std::env::var("AFT_FAKE_LSP_SERVER_NAME")
                                    .unwrap_or_else(|_| "fake-lsp-server".into()),
                                "version": "0.1.0",
                            }
                        }),
                    )?;
                    write_notification(
                        &mut writer,
                        &Notification::new(
                            "custom/initialized",
                            Some(json!({
                                "initializationOptions": params
                                    .as_ref()
                                    .and_then(|value| value.get("initializationOptions"))
                                    .cloned()
                                    .unwrap_or(Value::Null),
                                "serverStatusNotification": params
                                    .as_ref()
                                    .and_then(|value| value.pointer("/capabilities/experimental/serverStatusNotification"))
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                                "env": {
                                    "AFT_TEST_LSP_ENV": std::env::var("AFT_TEST_LSP_ENV").ok()
                                }
                            })),
                        ),
                    )?;
                }
                "shutdown" => {
                    if !ignore_shutdown {
                        write_response(&mut writer, id, Value::Null)?;
                    }
                }
                "workspace/executeCommand" => {
                    let params = params.as_ref().unwrap();
                    if let Some(path) = std::env::var_os("AFT_FAKE_LSP_TYPESCRIPT_SYNC_LOG") {
                        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                        writeln!(log, "{params}")?;
                    }
                    let args = &params["arguments"];
                    let command = args[0].as_str().unwrap_or_default();
                    let mode = std::env::var("AFT_FAKE_LSP_TYPESCRIPT_SYNC").unwrap_or_default();
                    if mode == "timeout" {
                        continue;
                    }
                    if mode == "unsupported" {
                        write_json_message(&mut writer, &json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": -32601, "message": "unsupported command" }
                        }))?;
                        continue;
                    }
                    assert_eq!(params["command"], "typescript.tsserverRequest");
                    assert_eq!(args[1]["includeLinePosition"], true);
                    assert_eq!(args[2]["executionTarget"], 0);
                    if mode == "malformed" {
                        write_response(&mut writer, id, Value::Null)?;
                        continue;
                    }
                    if mode == "categories" {
                        let category = match command {
                            "syntacticDiagnosticsSync" => "error",
                            "semanticDiagnosticsSync" => "warning",
                            "suggestionDiagnosticsSync" => "suggestion",
                            _ => panic!("unexpected diagnostics command: {command}"),
                        };
                        write_response(&mut writer, id, json!({
                            "type": "response", "command": command, "success": true,
                            "body": [{ "startLocation": { "line": 1, "offset": 1 },
                                "endLocation": { "line": 1, "offset": 3 },
                                "category": category, "code": 1000, "message": command }]
                        }))?;
                        continue;
                    }
                    let diagnostics = if command == "semanticDiagnosticsSync" && mode != "clean" {
                        opened_diagnostics().as_array().unwrap().iter().map(|d| json!({
                            "startLocation": {
                                "line": d["range"]["start"]["line"].as_u64().unwrap() + 1,
                                "offset": d["range"]["start"]["character"].as_u64().unwrap() + 1
                            },
                            "endLocation": {
                                "line": d["range"]["end"]["line"].as_u64().unwrap() + 1,
                                "offset": d["range"]["end"]["character"].as_u64().unwrap() + 1
                            },
                            "message": d["message"], "code": d["code"], "source": d["source"],
                            "category": if d["severity"] == 1 { "error" } else { "warning" }
                        })).collect::<Vec<_>>()
                    } else { Vec::new() };
                    write_response(&mut writer, id, json!({
                        "type": "response", "command": command, "success": true, "body": diagnostics
                    }))?;
                }
                "rust-analyzer/reloadWorkspace" => {
                    // rust-analyzer answers once the reload is queued, then
                    // reports the load through server status.
                    write_response(&mut writer, id, Value::Null)?;
                    write_notification(
                        &mut writer,
                        &Notification::new("custom/reloadWorkspace", None),
                    )?;
                    if cargo_lock_status_mode() {
                        // The load reads the lockfile when it finishes (see
                        // the timer above); until then, status repeats the
                        // previous load's result.
                        write_cargo_lock_status(&mut writer, last_load_stale, false)?;
                        reload_finishes_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_millis(300));
                    } else {
                        write_notification(
                            &mut writer,
                            &Notification::new(
                                "experimental/serverStatus",
                                Some(json!({
                                    "health": "ok",
                                    "quiescent": false,
                                    "message": "reloading the workspace",
                                })),
                            ),
                        )?;
                        write_notification(
                            &mut writer,
                            &Notification::new(
                                "experimental/serverStatus",
                                Some(json!({
                                    "health": "ok",
                                    "quiescent": true,
                                    "message": "workspace analysis is ready",
                                })),
                            ),
                        )?;
                    }
                }
                "textDocument/hover" => {
                    // AFT_FAKE_LSP_HOVER_DELAY_SIGNAL=<path> is written when a
                    // hover arrives and AFT_FAKE_LSP_HOVER_DELAY_MS=<ms> holds
                    // the reply, so a test can act while AFT waits on a slow
                    // server.
                    if let Some(signal_path) = std::env::var_os("AFT_FAKE_LSP_HOVER_DELAY_SIGNAL") {
                        let _ = std::fs::write(signal_path, b"hovering");
                    }
                    if let Some(delay_ms) = std::env::var("AFT_FAKE_LSP_HOVER_DELAY_MS")
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                    let (line, character) = request_position(&params);
                    if line == 0 && character == 0 {
                        write_response(
                            &mut writer,
                            id,
                            json!({
                                "contents": {
                                    "kind": "markdown",
                                    "value": "```typescript\nconst x: number\n```"
                                },
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 7 }
                                }
                            }),
                        )?;
                    } else {
                        write_response(&mut writer, id, Value::Null)?;
                    }
                }
                "textDocument/definition" => {
                    let uri = request_document_uri(&params);
                    write_response(
                        &mut writer,
                        id,
                        json!({
                            "uri": uri,
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 10 }
                            }
                        }),
                    )?;
                }
                "textDocument/references" => {
                    let uri = request_document_uri(&params);
                    let include_declaration = request_include_declaration(&params);
                    let mut locations = vec![json!({
                        "uri": uri.clone(),
                        "range": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 2, "character": 5 }
                        }
                    })];
                    if include_declaration {
                        locations.insert(
                            0,
                            json!({
                                "uri": uri,
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 5 }
                                }
                            }),
                        );
                    }
                    write_response(&mut writer, id, json!(locations))?;
                }
                "textDocument/prepareRename" => {
                    let (line, character) = request_position(&params);
                    if line == 0 && character == 4 {
                        write_response(
                            &mut writer,
                            id,
                            json!({
                                "range": {
                                    "start": { "line": 0, "character": 4 },
                                    "end": { "line": 0, "character": 9 }
                                },
                                "placeholder": "hello"
                            }),
                        )?;
                    } else {
                        write_response(&mut writer, id, Value::Null)?;
                    }
                }
                "textDocument/diagnostic" => {
                    if dynamic_pull {
                        record_pull_event(&format!("pull registered={diagnostic_registered} {}", request_document_uri_string(&params)))?;
                        if !diagnostic_registered {
                            write_json_message(&mut writer, &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"document diagnostics were not registered"}}))?;
                            continue;
                        }
                        if open_documents.get(&request_document_uri_string(&params)).is_some_and(|(items, _)| items.as_array().is_some_and(|items| items.is_empty())) {
                            write_response(&mut writer, id, json!({"kind":"full","resultId":"fake-clean-result","items":[]}))?;
                            continue;
                        }
                    }
                    // LSP 3.17 pull diagnostics. Honor previousResultId for
                    // unchanged-state replies. Fail-mode is controlled by
                    // env var so tests can drive each branch.
                    if let Some(signal_path) = std::env::var_os("AFT_FAKE_LSP_PULL_DELAY_SIGNAL") {
                        let _ = std::fs::write(signal_path, b"pulling");
                    }
                    if let Some(delay_ms) = std::env::var("AFT_FAKE_LSP_PULL_DELAY_MS")
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        // A file gate lets a test obtain an initial report, then
                        // hold subsequent pulls by creating the configured file.
                        let delay_enabled = std::env::var_os("AFT_FAKE_LSP_PULL_DELAY_WHEN_FILE_EXISTS")
                            .is_none_or(|path| std::path::Path::new(&path).exists());
                        if delay_enabled {
                            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        }
                    }
                    let force_unchanged =
                        std::env::var("AFT_FAKE_LSP_PULL_UNCHANGED").ok().as_deref() == Some("1");
                    let force_error =
                        std::env::var("AFT_FAKE_LSP_PULL_ERROR").ok().as_deref() == Some("1");
                    let force_method_not_found =
                        std::env::var("AFT_FAKE_LSP_PULL_METHOD_NOT_FOUND")
                            .ok()
                            .as_deref()
                            == Some("1");
                    let force_invalid_params = std::env::var("AFT_FAKE_LSP_PULL_INVALID_PARAMS")
                        .ok()
                        .as_deref()
                        == Some("1");

                    if std::env::var("AFT_FAKE_LSP_PULL_EXIT_MODULE_NOT_FOUND")
                        .ok()
                        .as_deref()
                        == Some("1")
                    {
                        eprintln!(
                            "Error: Cannot find module '/missing/typescript-language-server/lib/cli.mjs'"
                        );
                        eprintln!("code: 'MODULE_NOT_FOUND'");
                        std::process::exit(1);
                    }

                    let cancel_pull = pull_cancel_mode.as_deref() == Some("always")
                        || (pull_cancel_mode.as_deref() == Some("once") && !pull_cancelled_once);
                    if cancel_pull {
                        pull_cancelled_once = true;
                        write_json_message(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32802,
                                    "message": "server cancelled the request",
                                    "data": { "retriggerRequest": true }
                                }
                            }),
                        )?;
                        let uri = document_uri(&params);
                        let version = uri
                            .as_str()
                            .and_then(|uri| open_documents.get(uri))
                            .map(|(_, version)| version.clone())
                            .unwrap_or(Value::Null);
                        write_flycheck_progress(&mut writer, "begin")?;
                        write_publish_diagnostics_versioned(
                            &mut writer,
                            uri,
                            json!([fake_compile_error()]),
                            version,
                        )?;
                        write_flycheck_progress(&mut writer, "end")?;
                    } else if force_method_not_found || force_invalid_params {
                        let (code, message) = if force_method_not_found {
                            (-32601, "fake-lsp: pull method not found")
                        } else {
                            (-32602, "fake-lsp: invalid pull params")
                        };
                        write_json_message(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": code,
                                    "message": message
                                }
                            }),
                        )?;
                    } else if force_error {
                        write_json_message(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32603,
                                    "message": "fake-lsp: forced pull error"
                                }
                            }),
                        )?;
                    } else if std::env::var("AFT_FAKE_LSP_PULL_WAIT_FOR_CHECK")
                        .ok()
                        .as_deref()
                        == Some("1")
                        && !check_completed
                    {
                        // A successful native pull can describe the old
                        // workspace before a watched-file change is applied.
                        write_response(
                            &mut writer,
                            id,
                            json!({
                                "kind": "full", "resultId": "old-native", "items": []
                            }),
                        )?;
                    } else if force_unchanged {
                        write_response(
                            &mut writer,
                            id,
                            json!({
                                "kind": "unchanged",
                                "resultId": "fake-result-1"
                            }),
                        )?;
                    } else if std::env::var("AFT_FAKE_LSP_PULL_FROM_CONTENT").ok().as_deref()
                        == Some("1")
                    {
                        // AFT_FAKE_LSP_PULL_FROM_CONTENT=1: report one error
                        // for every line of the synced text that contains
                        // `BROKEN`, so a test can tell an analysis of the
                        // current text from one of an older text.
                        let text = document_uri(&params)
                            .as_str()
                            .and_then(|uri| document_texts.get(uri))
                            .cloned()
                            .unwrap_or_default();
                        let items = text
                            .lines()
                            .enumerate()
                            .filter(|(_, line)| line.contains("BROKEN"))
                            .map(|(line, _)| {
                                json!({
                                    "range": {
                                        "start": { "line": line, "character": 0 },
                                        "end": { "line": line, "character": 6 }
                                    },
                                    "severity": 1,
                                    "code": "E0TEXT",
                                    "source": "fake-lsp",
                                    "message": "BROKEN marker in synced text"
                                })
                            })
                            .collect::<Vec<_>>();
                        write_response(
                            &mut writer,
                            id,
                            json!({ "kind": "full", "items": items }),
                        )?;
                    } else {
                        write_response(
                            &mut writer,
                            id,
                            json!({
                                "kind": "full",
                                "resultId": "fake-result-1",
                                "items": [
                                    {
                                        "range": {
                                            "start": { "line": 4, "character": 0 },
                                            "end": { "line": 4, "character": 8 }
                                        },
                                        "severity": 1,
                                        "code": "E0PULL",
                                        "source": "fake-lsp",
                                        "message": "test pull diagnostic"
                                    }
                                ]
                            }),
                        )?;
                    }
                }
                "workspace/diagnostic" => {
                    // LSP 3.17 workspace pull. Fake produces one report for
                    // a synthetic URI. Test variants:
                    //   AFT_FAKE_LSP_WS_TIMEOUT=1 → never reply (server hangs)
                    //   AFT_FAKE_LSP_WS_PARTIAL=1 → reply with empty items
                    //   AFT_FAKE_LSP_WS_UNCHANGED=1 → unchanged reports with fresh result IDs
                    let force_timeout =
                        std::env::var("AFT_FAKE_LSP_WS_TIMEOUT").ok().as_deref() == Some("1");
                    let force_partial =
                        std::env::var("AFT_FAKE_LSP_WS_PARTIAL").ok().as_deref() == Some("1");
                    let force_unchanged =
                        std::env::var("AFT_FAKE_LSP_WS_UNCHANGED").ok().as_deref() == Some("1");

                    if force_timeout {
                        // Never respond — emulates a server still analyzing.
                        // The client should hit its 10s timeout and cancel.
                        continue;
                    }

                    let items = if force_partial {
                        json!([])
                    } else if force_unchanged {
                        let previous = params
                            .as_ref()
                            .and_then(|value| value.get("previousResultIds"))
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        Value::Array(previous.into_iter().map(|previous| json!({
                            "kind": "unchanged", "uri": previous["uri"],
                            "resultId": format!("next-{}", previous["value"].as_str().unwrap_or_default()), "version": 99
                        })).collect())
                    } else {
                        json!([{
                            "kind": "full",
                            "uri": "file:///workspace-pull-target.ts",
                            "resultId": "fake-ws-1",
                            "items": [
                                {
                                    "range": {
                                        "start": { "line": 7, "character": 0 },
                                        "end": { "line": 7, "character": 5 }
                                    },
                                    "severity": 1,
                                    "code": "E0WSP",
                                    "source": "fake-lsp",
                                    "message": "workspace pull diagnostic"
                                }
                            ]
                        }])
                    };

                    write_response(&mut writer, id, json!({ "items": items }))?;
                }
                "textDocument/rename" => {
                    let uri_key = request_document_uri_string(&params);
                    let new_name = request_new_name(&params);
                    let edits = if new_name == "__force_failure__" {
                        vec![
                            json!({
                                "range": {
                                    "start": { "line": 0, "character": 4 },
                                    "end": { "line": 0, "character": 9 }
                                },
                                "newText": new_name
                            }),
                            json!({
                                "range": {
                                    "start": { "line": 99, "character": 0 },
                                    "end": { "line": 99, "character": 5 }
                                },
                                "newText": new_name
                            }),
                        ]
                    } else {
                        vec![
                            json!({
                                "range": {
                                    "start": { "line": 0, "character": 4 },
                                    "end": { "line": 0, "character": 9 }
                                },
                                "newText": new_name
                            }),
                            json!({
                                "range": {
                                    "start": { "line": 2, "character": 0 },
                                    "end": { "line": 2, "character": 5 }
                                },
                                "newText": new_name
                            }),
                        ]
                    };
                    let mut changes = serde_json::Map::new();
                    changes.insert(uri_key, Value::Array(edits));
                    write_response(
                        &mut writer,
                        id,
                        Value::Object(
                            [("changes".to_string(), Value::Object(changes))]
                                .into_iter()
                                .collect(),
                        ),
                    )?;
                }
                _ => {
                    write_json_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32601,
                                "message": format!("method not found: {method}"),
                            }
                        }),
                    )?;
                }
            },
            ServerMessage::Notification { method, params } => match method.as_str() {
                "initialized" => {
                    if dynamic_pull && client_dynamic_pull {
                        if let Some(delay) = std::env::var("AFT_FAKE_LSP_REGISTER_DIAGNOSTICS_DELAY_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
                            std::thread::sleep(std::time::Duration::from_millis(delay));
                        }
                        write_request(&mut writer, 22_000, "client/registerCapability", json!({"registrations":[{"id":"fake-python-diagnostics","method":"textDocument/diagnostic","registerOptions":{"documentSelector":null,"identifier":"fake-lsp","interFileDependencies":true,"workspaceDiagnostics":false}}]}))?;
                    }
                    if std::env::var("AFT_FAKE_LSP_ANALYSIS_PROGRESS").ok().as_deref() == Some("already_running") {
                        write_request(&mut writer, 20_000, "window/workDoneProgress/create", json!({"token": 17}))?;
                        write_notification(&mut writer, &Notification::new("$/progress", Some(json!({"token": 17, "value": {"kind": "begin", "title": "Analysis"}}))))?;
                    }
                    let server_status_mode = std::env::var("AFT_FAKE_LSP_SERVER_STATUS").ok();
                    if cargo_lock_status_mode() {
                        last_load_stale = cargo_lock_is_stale(workspace_root.as_deref());
                        write_cargo_lock_status(&mut writer, last_load_stale, true)?;
                    } else if server_status_mode.as_deref() != Some("disabled") {
                        let warming = matches!(
                            server_status_mode.as_deref(),
                            Some("1" | "publish_then_quiescent" | "empty_then_quiescent")
                        );
                        write_notification(
                            &mut writer,
                            &Notification::new(
                                "experimental/serverStatus",
                                Some(json!({
                                    "health": if warming || server_status_mode.as_deref() == Some("warning") { "warning" } else { "ok" },
                                    "quiescent": !warming,
                                    "message": if server_status_mode.as_deref() == Some("warning") {
                                        "proc-macro server failed to start"
                                    } else if warming {
                                        "workspace analysis is warming"
                                    } else {
                                        "workspace analysis is ready"
                                    },
                                })),
                            ),
                        )?;
                    }
                    if let Some(mode) = flycheck_mode.as_deref() {
                        // Keep quiescence observable before progress begins so
                        // inspect tests can exercise that event boundary without
                        // generating CPU load on a shared test machine.
                        if let Some(delay_ms) =
                            std::env::var("AFT_FAKE_LSP_FLYCHECK_BEGIN_DELAY_MS")
                                .ok()
                                .and_then(|value| value.parse::<u64>().ok())
                        {
                            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        }
                        write_flycheck_progress(&mut writer, "begin")?;
                        flycheck_running = mode != "never";
                    }
                    if let Some(at) = schedule_check("load") {
                        check_begins_at = Some(at);
                    }
                    if should_register_watched_files {
                        // AFT_FAKE_LSP_WATCHED_GLOBS replaces the catch-all
                        // watcher with a JSON array of FileSystemWatcher values.
                        let watchers = std::env::var("AFT_FAKE_LSP_WATCHED_GLOBS")
                            .ok()
                            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                            .unwrap_or_else(|| json!([{ "globPattern": "**/*" }]));
                        write_request(
                            &mut writer,
                            10_000,
                            "client/registerCapability",
                            json!({
                                "registrations": [
                                    {
                                        "id": "fake-lsp-watched-files",
                                        "method": "workspace/didChangeWatchedFiles",
                                        "registerOptions": {
                                            "watchers": watchers
                                        }
                                    }
                                ]
                            }),
                        )?;
                        should_register_watched_files = false;
                    }
                }
                "workspace/didChangeWatchedFiles" => {
                    if dynamic_pull && diagnostic_registered {
                        if std::env::var_os("AFT_FAKE_LSP_UNREGISTER_DIAGNOSTICS").is_some() {
                            write_request(&mut writer, 22_002, "client/unregisterCapability", json!({"unregisterations":[{"id":"fake-python-diagnostics","method":"textDocument/diagnostic"}]}))?;
                        } else {
                            for (items, _) in open_documents.values_mut() { *items = json!([]); }
                            record_pull_event("refresh-requested")?;
                            write_request(&mut writer, 22_001, "workspace/diagnostic/refresh", json!(null))?;
                        }
                    }
                    write_notification(
                        &mut writer,
                        &Notification::new("custom/watchedFilesChanged", params),
                    )?;
                }
                "textDocument/didOpen" => {
                    let uri = document_uri(&params);
                    let version = document_version(&params);
                    if let (Some(uri), Some(text)) = (
                        uri.as_str(),
                        params
                            .as_ref()
                            .and_then(|p| p.pointer("/textDocument/text"))
                            .and_then(Value::as_str),
                    ) {
                        document_texts.insert(uri.to_string(), text.to_string());
                    }
                    if dynamic_pull {
                        if let Some(uri) = uri.as_str() { open_documents.insert(uri.to_string(), (opened_diagnostics(), version.clone())); }
                        record_pull_event(&format!("open {uri}"))?;
                        write_publish_diagnostics_versioned(&mut writer, uri.clone(), if client_disables_pull { opened_diagnostics() } else { json!([]) }, version)?;
                        if client_dynamic_pull && diagnostic_registered { write_request(&mut writer, 22_001, "workspace/diagnostic/refresh", json!(null))?; }
                        continue;
                    }
                    if let Ok(mode) = std::env::var("AFT_FAKE_LSP_ANALYSIS_PROGRESS") {
                        analysis_opens += 1;
                        if let Some(release) = std::env::var_os("AFT_FAKE_LSP_BEFORE_ANALYSIS_PUBLISH") {
                            write_custom_notification(&mut writer, "custom/beforeAnalysisPublish", uri.clone(), version.clone())?;
                            while !std::path::Path::new(&release).exists() { std::thread::sleep(std::time::Duration::from_millis(10)); }
                        }
                        if mode == "short_no_publish" && analysis_opens > 1 { continue; }
                        if mode == "short_runs" && analysis_opens > 1 {
                            write_publish_diagnostics_versioned(&mut writer, uri, opened_diagnostics(), version)?;
                            continue;
                        }
                        // Model an empty startup report arriving before the
                        // server's background analysis has even started.
                        if mode != "end_before_publication" {
                            write_publish_diagnostics_versioned(&mut writer, uri.clone(), json!([]), version.clone())?;
                        }
                        if mode != "already_running" {
                            write_request(&mut writer, 20_000, "window/workDoneProgress/create", json!({"token": 17}))?;
                            if mode == "announce_delay" { std::thread::sleep(std::time::Duration::from_millis(2500)); }
                            write_notification(&mut writer, &Notification::new("$/progress", Some(json!({"token": 17, "value": {"kind": "begin", "title": "Analysis"}}))))?;
                        }
                        if mode == "end_before_publication" {
                            write_notification(&mut writer, &Notification::new("$/progress", Some(json!({"token": 17, "value": {"kind": "end"}}))))?;
                            write_publish_diagnostics_versioned(&mut writer, uri, opened_diagnostics(), version)?;
                            continue;
                        }
                        if mode == "new_begin" {
                            write_publish_diagnostics_versioned(&mut writer, uri.clone(), opened_diagnostics(), version.clone())?;
                            write_notification(&mut writer, &Notification::new("$/progress", Some(json!({"token": "next", "value": {"kind": "begin", "title": "Analysis"}}))))?;
                            write_notification(&mut writer, &Notification::new("$/progress", Some(json!({"token": 17, "value": {"kind": "end"}}))))?;
                            std::thread::sleep(std::time::Duration::from_millis(600));
                            write_publish_diagnostics_versioned(&mut writer, uri, changed_diagnostics(), version)?;
                            write_notification(&mut writer, &Notification::new("$/progress", Some(json!({"token": "next", "value": {"kind": "end"}}))))?;
                            continue;
                        }
                        if mode != "never" {
                            if let Some(release) = std::env::var_os("AFT_FAKE_LSP_ANALYSIS_RELEASE_FILE") {
                                while !std::path::Path::new(&release).exists() {
                                    std::thread::sleep(std::time::Duration::from_millis(10));
                                }
                            }
                            std::thread::sleep(std::time::Duration::from_millis(600));
                            write_publish_diagnostics_versioned(&mut writer, uri, opened_diagnostics(), version)?;
                            write_notification(&mut writer, &Notification::new("$/progress", Some(json!({"token": 17, "value": {"kind": "end"}}))))?;
                        }
                        continue;
                    }
                    if let Some(uri) = uri.as_str() {
                        open_documents
                            .insert(uri.to_string(), (opened_diagnostics(), version.clone()));
                    }

                    write_custom_notification(
                        &mut writer,
                        "custom/documentOpened",
                        uri.clone(),
                        version.clone(),
                    )?;
                    if std::env::var_os("AFT_FAKE_LSP_TYPESCRIPT_SYNC").is_some() {
                        // TypeScript can publish syntax diagnostics before its
                        // semantic pass. Only the explicit request certifies
                        // that all diagnostic categories have been collected.
                        write_publish_diagnostics(&mut writer, uri.clone(), json!([]))?;
                        if std::env::var("AFT_FAKE_LSP_TYPESCRIPT_SYNC").as_deref() == Ok("empty_then_real") {
                            std::thread::sleep(std::time::Duration::from_millis(300));
                            write_publish_diagnostics(&mut writer, uri.clone(), opened_diagnostics())?;
                        }
                        continue;
                    }
                    // AFT_FAKE_LSP_OPEN_PUBLISH_DELAY_MS=<ms>: publish an opened
                    // document's diagnostics late, as a server that analyzes
                    // in the background (tsserver) does.
                    if let Some(delay_ms) = std::env::var("AFT_FAKE_LSP_OPEN_PUBLISH_DELAY_MS")
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                    if push_diagnostics_enabled() {
                        let mut diagnostics = opened_diagnostics();
                        if uri.as_str().is_some_and(|uri| check_errors.contains(uri)) {
                            diagnostics
                                .as_array_mut()
                                .expect("diagnostics are an array")
                                .push(fake_compile_error());
                        }
                        write_publish_diagnostics_versioned(
                            &mut writer,
                            uri.clone(),
                            diagnostics,
                            version.clone(),
                        )?;
                    } else if std::env::var("AFT_FAKE_LSP_PULL_FROM_CONTENT").ok().as_deref()
                        != Some("1")
                    {
                        // A pull-only server (`tsc --lsp`) publishes nothing
                        // for an opened source file; the content mode models it.
                        write_publish_diagnostics(&mut writer, uri.clone(), json!([]))?;
                    }
                    if matches!(
                        std::env::var("AFT_FAKE_LSP_SERVER_STATUS").ok().as_deref(),
                        Some("publish_then_quiescent" | "empty_then_quiescent")
                    ) {
                        if let Some(release) = std::env::var_os("AFT_FAKE_LSP_QUIESCENCE_RELEASE_FILE") {
                            while !std::path::Path::new(&release).exists() {
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                        }
                        write_notification(
                            &mut writer,
                            &Notification::new(
                                "experimental/serverStatus",
                                Some(json!({
                                    "health": "ok",
                                    "quiescent": true,
                                    "message": "workspace analysis is ready",
                                })),
                            ),
                        )?;
                    }
                    if flycheck_running {
                        flycheck_running = false;
                        let delay_ms = flycheck_mode
                            .as_deref()
                            .and_then(|mode| mode.parse::<u64>().ok())
                            .unwrap_or(0);
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        let mut checked = opened_diagnostics();
                        checked
                            .as_array_mut()
                            .expect("opened diagnostics are an array")
                            .push(json!({
                                "range": {
                                    "start": { "line": 3, "character": 0 },
                                    "end": { "line": 3, "character": 4 }
                                },
                                "severity": 1,
                                "code": "E0063",
                                "source": "rustc",
                                "message": "flycheck diagnostic"
                            }));
                        write_publish_diagnostics_versioned(&mut writer, uri, checked, version)?;
                        write_flycheck_progress(&mut writer, "end")?;
                    }
                    // Stop reading after a successful handshake and document open.
                    // The release file lets a test resume the same process without
                    // relying on platform-specific signals or leaving a stopped child.
                    if let Some(release) = std::env::var_os("AFT_FAKE_LSP_FREEZE_AFTER_OPEN") {
                        write_notification(&mut writer, &Notification::new("custom/frozen", None))?;
                        while !std::path::Path::new(&release).exists() {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        write_notification(
                            &mut writer,
                            &Notification::new("custom/resumed", None),
                        )?;
                    }
                }
                "textDocument/didChange" => {
                    if let (Some(uri), Some(text)) = (
                        document_uri(&params).as_str(),
                        params
                            .as_ref()
                            .and_then(|p| p.get("contentChanges"))
                            .and_then(Value::as_array)
                            .and_then(|changes| changes.last())
                            .and_then(|change| change.get("text"))
                            .and_then(Value::as_str),
                    ) {
                        document_texts.insert(uri.to_string(), text.to_string());
                    }
                    if dynamic_pull && client_dynamic_pull {
                        let uri = document_uri(&params);
                        if let Some(uri) = uri.as_str() { open_documents.insert(uri.into(), (changed_diagnostics(), document_version(&params))); }
                        write_request(&mut writer, 22_001, "workspace/diagnostic/refresh", json!(null))?;
                        continue;
                    }
                    if let Some(path) = std::env::var_os("AFT_FAKE_LSP_DID_CHANGE_LOG") {
                        let mut log = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                        writeln!(log, "{}", serde_json::to_string(&params)?)?;
                    }
                    let uri = document_uri(&params);
                    let version = document_version(&params);
                    if let Some(uri) = uri.as_str() {
                        open_documents
                            .insert(uri.to_string(), (changed_diagnostics(), version.clone()));
                    }
                    delay_changed_diagnostics_if_requested();
                    if std::env::var("AFT_FAKE_LSP_ECHO_CHANGES").ok().as_deref() == Some("1") {
                        write_notification(
                            &mut writer,
                            &Notification::new("custom/documentChangedContents", params.clone()),
                        )?;
                    }
                    if std::env::var("AFT_FAKE_LSP_SERVER_STATUS").ok().as_deref() == Some("1") {
                        write_notification(
                            &mut writer,
                            &Notification::new(
                                "experimental/serverStatus",
                                Some(json!({
                                    "health": "ok",
                                    "quiescent": true,
                                    "message": "workspace analysis is ready",
                                })),
                            ),
                        )?;
                    }
                    write_custom_notification(
                        &mut writer,
                        "custom/documentChanged",
                        uri.clone(),
                        version.clone(),
                    )?;
                    if push_diagnostics_enabled() {
                        let mut diagnostics = changed_diagnostics();
                        if uri.as_str().is_some_and(|uri| check_errors.contains(uri)) {
                            diagnostics
                                .as_array_mut()
                                .expect("diagnostics are an array")
                                .push(fake_compile_error());
                        }
                        write_publish_diagnostics_versioned(
                            &mut writer,
                            uri,
                            diagnostics,
                            version,
                        )?;
                    }
                }
                "textDocument/didSave" => {
                    if let Some(at) = schedule_check("save") {
                        check_begins_at = Some(at);
                    }
                }
                "rust-analyzer/runFlycheck" => {
                    if let Some(at) = schedule_check("run") {
                        check_begins_at = Some(at);
                    }
                }
                "textDocument/didClose" => {
                    let uri = document_uri(&params);
                    if let Some(uri) = uri.as_str() {
                        open_documents.remove(uri);
                        document_texts.remove(uri);
                    }
                    write_custom_notification(
                        &mut writer,
                        "custom/documentClosed",
                        uri.clone(),
                        Value::Null,
                    )?;
                    // AFT_FAKE_LSP_CLOSE_DELAY_MS=<ms>: answer the close late.
                    if let Some(delay_ms) = std::env::var("AFT_FAKE_LSP_CLOSE_DELAY_MS")
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                    // AFT_FAKE_LSP_CLOSE_PUBLISHES=1: answer the close with real
                    // diagnostics instead of the usual clearing empty list.
                    // A pull-only server (AFT_FAKE_LSP_PULL_FROM_CONTENT=1, as
                    // `tsc --lsp`) publishes nothing for a closed source file,
                    // unless AFT_FAKE_LSP_PULL_CONTENT_CLEARS_ON_CLOSE=1 makes
                    // it send the usual clearing empty list.
                    if std::env::var("AFT_FAKE_LSP_PULL_FROM_CONTENT").ok().as_deref()
                        == Some("1")
                        && std::env::var("AFT_FAKE_LSP_PULL_CONTENT_CLEARS_ON_CLOSE")
                            .ok()
                            .as_deref()
                            != Some("1")
                    {
                        continue;
                    }
                    let close_diagnostics = if std::env::var("AFT_FAKE_LSP_CLOSE_PUBLISHES")
                        .ok()
                        .as_deref()
                        == Some("1")
                    {
                        changed_diagnostics()
                    } else {
                        json!([])
                    };
                    write_publish_diagnostics(&mut writer, uri, close_diagnostics)?;
                }
                "exit" => break,
                _ => {}
            },
            ServerMessage::Response(response) => {
                if dynamic_pull && response.id == RequestId::Int(22_000) && response.error.is_none() {
                    diagnostic_registered = true;
                    record_pull_event("register-accepted")?;
                }
                if dynamic_pull && response.id == RequestId::Int(22_002) && response.error.is_none() {
                    diagnostic_registered = false;
                    record_pull_event("unregister-accepted")?;
                    write_notification(&mut writer, &Notification::new("custom/diagnosticsUnregistered", None))?;
                }
            }
        }
    }

    // AFT_FAKE_LSP_EXIT_DELAY_MS=<ms>: linger after the client goes away
    // before exiting, like a server that finishes its own work first. Only a
    // kill ends such a server promptly.
    if let Some(delay_ms) = std::env::var("AFT_FAKE_LSP_EXIT_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }

    Ok(())
}

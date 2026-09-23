use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[test]
fn spawned_aft_writes_durable_log_under_aft_cache_dir() {
    let temp = tempfile::TempDir::new().unwrap();
    let binary = std::env::var_os("NEXTEST_BIN_EXE_aft")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")));
    let mut child = Command::new(binary)
        .env("AFT_CACHE_DIR", temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"id\":\"logging-smoke\",\"command\":\"echo\",\"message\":\"ok\"}\n")
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log_path = temp
        .path()
        .join("aft")
        .join("logs")
        .join(format!("aft-{pid}.log"));
    for _ in 0..20 {
        if std::fs::read_to_string(&log_path)
            .is_ok_and(|contents| contents.contains("started, pid"))
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("durable log was not written at {}", log_path.display());
}

#[test]
fn malformed_request_body_leaves_no_trace_in_the_log() {
    let temp = tempfile::TempDir::new().unwrap();
    let binary = std::env::var_os("NEXTEST_BIN_EXE_aft")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")));
    let mut child = Command::new(binary)
        .env("AFT_CACHE_DIR", temp.path())
        // Slow the log writer so the lines logged just before exit are still
        // queued when the process ends: the test then fails unless every exit
        // path waits for the writer to drain.
        .env("AFT_TEST_LOG_WRITER_DELAY_MS", "300")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    // The closing brace is missing, so the whole line fails to parse. It
    // carries shell text and file content that must not be copied to the log.
    let command_marker = "curl -u admin:CMD_MARKER_5150 https://internal.example";
    let content_marker = "FILE_CONTENT_MARKER_7331";
    let malformed = format!(
        "{{\"id\":\"p1\",\"command\":\"bash\",\"params\":{{\"command\":\"{command_marker}\"}},\"content\":\"{content_marker}\"\n"
    );
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(malformed.as_bytes()).unwrap();
    stdin
        .write_all(b"{\"id\":\"after\",\"command\":\"echo\",\"message\":\"ok\"}\n")
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    let log_path = temp
        .path()
        .join("aft")
        .join("logs")
        .join(format!("aft-{pid}.log"));
    // No polling: the process has exited, so everything it logged must
    // already be on disk.
    let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
    // The parse-error line must exist, or the absence checks prove nothing.
    let parse_line = contents
        .lines()
        .find(|line| line.contains("parse error"))
        .unwrap_or_else(|| panic!("no parse error line in {}:\n{contents}", log_path.display()));
    let expected_bytes = format!("bytes={}", malformed.trim_end().len());
    assert!(parse_line.contains(&expected_bytes), "{parse_line}");
    assert!(parse_line.contains("sha256="), "{parse_line}");
    for marker in ["CMD_MARKER_5150", content_marker, "internal.example"] {
        assert!(
            !contents.contains(marker),
            "log file leaked {marker}:\n{contents}"
        );
        assert!(
            !stderr.contains(marker),
            "stderr leaked {marker}:\n{stderr}"
        );
    }
}

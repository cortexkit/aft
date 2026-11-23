//! A one-shot CLI command must not end while a background thread is inside
//! ONNX Runtime. Ending runs ONNX Runtime's C++ static destructors, and a
//! meaning-search fill still creating its embedding session then reads freed
//! memory and the process dies with SIGSEGV after its output is complete.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::test_helpers::aft_binary;

/// A throwaway HOME, storage folder and one-file project, plus the folder a
/// test hook uses for its `entered`, `release` and `left` files.
struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    storage: PathBuf,
    root: PathBuf,
    hold: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let storage = temp.path().join("storage");
        let root = temp.path().join("root");
        let hold = temp.path().join("hold");
        for dir in [&home, &storage, &root.join("src"), &hold] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn greet() -> &'static str {\n    \"hello\"\n}\n",
        )
        .unwrap();
        Self {
            _temp: temp,
            home,
            storage,
            root,
            hold,
        }
    }

    /// Starts `aft warmup --root <root> <args>` with `hook` set to the hold
    /// folder. Returns the child and every line it writes to stdout or stderr.
    fn spawn_warmup(&self, args: &[&str], hook: &str) -> (Child, mpsc::Receiver<String>) {
        let mut child = Command::new(aft_binary::aft_binary())
            .arg("warmup")
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .env_clear()
            // Keep the search path, and on Windows the system folder: AFT needs
            // git to decide whether the root is a repository, and on Windows it
            // cannot find git, or run it, without them. The meaning-search fill
            // then never starts and the test never reaches the exit it checks.
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(std::env::var_os("SystemRoot").map(|value| ("SystemRoot", value)))
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("APPDATA", &self.home)
            .env("LOCALAPPDATA", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.home)
            .env("XDG_DATA_HOME", &self.home)
            .env("XDG_STATE_HOME", &self.home)
            .env("AFT_STORAGE_DIR", &self.storage)
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            .env(hook, &self.hold)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (lines_tx, lines_rx) = mpsc::channel();
        forward_lines(child.stdout.take().unwrap(), lines_tx.clone());
        forward_lines(child.stderr.take().unwrap(), lines_tx);
        (child, lines_rx)
    }
}

fn forward_lines(stream: impl Read + Send + 'static, lines: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let _ = lines.send(line);
        }
    });
}

fn wait_for_file(path: &Path, child: &mut Child, lines: &mpsc::Receiver<String>, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            // Give the reader a moment to pass on the last lines aft wrote.
            std::thread::sleep(Duration::from_millis(200));
            let stderr: Vec<String> = lines.try_iter().collect();
            panic!("aft exited with {status} before {what}; stderr: {stderr:#?}");
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Reads output lines until one starts with `prefix`; returns every line read.
fn wait_for_line(lines: &mpsc::Receiver<String>, child: &mut Child, prefix: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(remaining) {
            Ok(line) => {
                let done = line.starts_with(prefix);
                seen.push(line);
                if done {
                    return seen;
                }
            }
            Err(error) => {
                let _ = child.kill();
                panic!("no line starting {prefix:?} ({error}); output so far: {seen:#?}");
            }
        }
    }
}

/// After the command's last output line: the process must still be running
/// while the hold keeps its ONNX Runtime section open, and must end with
/// `expected_code` only after the section has been left.
fn assert_exit_waits_for_the_section(
    child: &mut Child,
    lines: &mpsc::Receiver<String>,
    mut seen: Vec<String>,
    hold: &Path,
    expected_code: i32,
) {
    // Without the quiesce the process ends within milliseconds of its last
    // output line, while the section is still open.
    std::thread::sleep(Duration::from_secs(1));
    let early = child.try_wait().unwrap();
    if early.is_some() {
        seen.extend(lines.try_iter());
        panic!(
            "warmup exited ({early:?}) while a thread was inside an ONNX Runtime section; \
             output: {seen:#?}"
        );
    }
    assert!(
        !hold.join("left").exists(),
        "the hold left its section before it was released"
    );

    std::fs::write(hold.join("release"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("warmup did not exit after the hold was released");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // The command's exit code is unchanged.
    assert_eq!(status.code(), Some(expected_code), "{status}");
    assert!(
        hold.join("left").exists(),
        "the process ended before the hold finished its ONNX Runtime section"
    );
}

#[test]
fn warmup_exit_waits_for_a_fill_inside_model_creation() {
    let fixture = Fixture::new();
    // The fill holds inside the section until `release` exists, so warmup
    // never sees the meaning-search fill complete and ends through its timeout.
    let (mut child, lines) = fixture.spawn_warmup(
        &["--only", "semantic", "--timeout", "2000"],
        "AFT_TEST_HOLD_FILL_MODEL_CREATION",
    );
    wait_for_file(
        &fixture.hold.join("entered"),
        &mut child,
        &lines,
        "the fill entered model creation",
    );
    // The timeout message is printed on the way out, before the exit path.
    let seen = wait_for_line(&lines, &mut child, "aft warmup timed out");
    assert_exit_waits_for_the_section(&mut child, &lines, seen, &fixture.hold, 1);
}

#[test]
fn successful_warmup_returns_only_after_an_open_section_ends() {
    let fixture = Fixture::new();
    // The hook opens an ONNX Runtime section before the command runs and
    // holds it until `release` exists. Warming only the search index needs
    // no ONNX Runtime, so the command itself succeeds quickly and the process
    // ends through the success path (exit code 0).
    let (mut child, lines) = fixture.spawn_warmup(
        &["--only", "search", "--timeout", "60000"],
        "AFT_TEST_HOLD_ORT_SECTION_AT_EXIT",
    );
    wait_for_file(
        &fixture.hold.join("entered"),
        &mut child,
        &lines,
        "the hook opened its section",
    );
    let seen = wait_for_line(&lines, &mut child, "aft warmup: ready");
    assert_exit_waits_for_the_section(&mut child, &lines, seen, &fixture.hold, 0);
}

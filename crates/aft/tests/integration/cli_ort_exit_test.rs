//! A one-shot CLI command must not end while a background thread is inside
//! ONNX Runtime. Ending runs ONNX Runtime's C++ static destructors, and a
//! meaning-search fill still creating its embedding session then reads freed
//! memory and the process dies with SIGSEGV after its output is complete.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::test_helpers::aft_binary;

fn wait_for_file(path: &Path, child: &mut Child, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("aft exited with {status} before {what}");
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn warmup_exit_waits_for_a_fill_inside_model_creation() {
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

    // The fill holds inside the section until `release` exists, so warmup
    // never sees the meaning plane complete and ends through its timeout.
    let mut child = Command::new(aft_binary::aft_binary())
        .arg("warmup")
        .arg("--root")
        .arg(&root)
        .args(["--only", "semantic", "--timeout", "2000"])
        .env_clear()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("APPDATA", &home)
        .env("LOCALAPPDATA", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("XDG_CACHE_HOME", &home)
        .env("XDG_DATA_HOME", &home)
        .env("XDG_STATE_HOME", &home)
        .env("AFT_STORAGE_DIR", &storage)
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env("AFT_TEST_HOLD_FILL_MODEL_CREATION", &hold)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (lines_tx, lines_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = lines_tx.send(line);
        }
    });

    wait_for_file(
        &hold.join("entered"),
        &mut child,
        "the fill entered model creation",
    );

    // The timeout message is printed on the way out, before the exit path.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match lines_rx.recv_timeout(remaining) {
            Ok(line) => {
                let done = line.starts_with("aft warmup timed out");
                seen.push(line);
                if done {
                    break;
                }
            }
            Err(error) => {
                let _ = child.kill();
                panic!("no warmup timeout message ({error}); stderr so far: {seen:#?}");
            }
        }
    }

    // Without the quiesce the process ends within milliseconds of that
    // message, while the fill is still inside the section.
    std::thread::sleep(Duration::from_secs(1));
    let early = child.try_wait().unwrap();
    if early.is_some() {
        reader.join().unwrap();
        seen.extend(lines_rx.try_iter());
        panic!(
            "warmup exited ({early:?}) while a fill was inside ONNX Runtime model creation; \
             stderr: {seen:#?}"
        );
    }
    assert!(
        !hold.join("left").exists(),
        "the fill left the section before it was released"
    );

    std::fs::write(hold.join("release"), b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("warmup did not exit after the fill was released");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    reader.join().unwrap();
    // The exit code of a timed-out warmup is unchanged.
    assert_eq!(status.code(), Some(1), "{status}");
    assert!(
        hold.join("left").exists(),
        "the process ended before the fill finished its ONNX Runtime section"
    );
}

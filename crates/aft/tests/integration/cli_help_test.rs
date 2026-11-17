use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::test_helpers::aft_binary;

struct Invocation {
    root: tempfile::TempDir,
    output: Output,
}

fn run(args: &[&str]) -> Invocation {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let storage = root.path().join("storage");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&storage).unwrap();
    let mut child = Command::new(aft_binary::aft_binary())
        .args(args)
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
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("aft {args:?} did not exit: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Invocation {
        root,
        output: child.wait_with_output().unwrap(),
    }
}

fn assert_empty(path: &Path) {
    let entries: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "{} contains {entries:?}",
        path.display()
    );
}

fn assert_no_startup_files(invocation: &Invocation) {
    assert_empty(&invocation.root.path().join("home"));
    assert_empty(&invocation.root.path().join("storage"));
    assert_eq!(
        std::fs::read_dir(invocation.root.path()).unwrap().count(),
        2
    );
}

fn assert_usage(text: &str) {
    assert!(text.starts_with("AFT — "), "{text}");
    assert!(text.contains("Usage: aft"), "{text}");
    assert!(text.contains("harness plugin"), "{text}");
    assert!(text.contains("CortexKit daemon"), "{text}");
    assert!(text.contains("--subc <connection-file>"), "{text}");
    for name in [
        "--version",
        "--formats",
        "--help",
        "setup",
        "fix-config",
        "index",
        "warmup",
        "profile",
        "backups",
        "cache",
        "migrate-storage",
        "clean-old-caches",
    ] {
        assert!(
            text.lines().any(|line| line.trim_start().starts_with(name)),
            "usage is missing {name}: {text}"
        );
    }
}

#[test]
fn help_flags_print_usage_without_startup_files() {
    let mut previous = None;
    for flag in ["--help", "-h"] {
        let invocation = run(&[flag]);
        assert_eq!(invocation.output.status.code(), Some(0));
        assert!(invocation.output.stderr.is_empty());
        let stdout = String::from_utf8(invocation.output.stdout.clone()).unwrap();
        assert_usage(&stdout);
        if let Some(previous) = &previous {
            assert_eq!(&stdout, previous);
        }
        previous = Some(stdout);
        assert_no_startup_files(&invocation);
    }
}

#[test]
fn unknown_option_prints_error_and_usage_without_startup_files() {
    let invocation = run(&["--frobnicate"]);
    assert_eq!(invocation.output.status.code(), Some(2));
    assert!(invocation.output.stdout.is_empty());
    let stderr = String::from_utf8(invocation.output.stderr.clone()).unwrap();
    let usage = stderr
        .strip_prefix("aft: unknown option '--frobnicate'\n\n")
        .unwrap_or_else(|| panic!("missing unknown-option diagnostic: {stderr}"));
    assert_usage(usage);
    let help = run(&["--help"]);
    assert_eq!(usage.as_bytes(), help.output.stdout);
    assert_no_startup_files(&invocation);
    assert_no_startup_files(&help);
}

#[test]
fn existing_metadata_flags_and_subc_launch_are_preserved() {
    for flag in ["--version", "-V"] {
        let invocation = run(&[flag]);
        assert_eq!(invocation.output.status.code(), Some(0));
        assert_eq!(
            invocation.output.stdout,
            format!("aft {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
        );
        assert!(invocation.output.stderr.is_empty());
        assert_no_startup_files(&invocation);
    }
    let formats = run(&["--formats"]);
    assert_eq!(formats.output.status.code(), Some(0));
    let actual: serde_json::Value = serde_json::from_slice(&formats.output.stdout).unwrap();
    assert_eq!(actual, aft::persisted_format::formats_json());
    assert!(actual.as_object().is_some_and(|object| !object.is_empty()));
    assert!(formats.output.stderr.is_empty());
    assert_no_startup_files(&formats);

    // A missing connection file must reach daemon attach, not standalone mode
    // or the unknown-option guard. No running daemon is needed for this check.
    for args in [
        vec!["--subc", "missing-connection.json"],
        vec!["--subc=missing-connection.json"],
    ] {
        let invocation = run(&args);
        assert_eq!(invocation.output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&invocation.output.stderr);
        assert!(stderr.contains("subc attach failed:"), "{stderr}");
        assert!(stderr.contains("missing-connection.json"), "{stderr}");
        assert!(!stderr.contains("unknown option"), "{stderr}");
    }
}

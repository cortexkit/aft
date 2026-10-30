use super::*;

const FIXTURE: &str = include_str!("../../tests/fixtures/perf_sweep.txt");
const SINCE: u64 = 1_735_689_600_000;
fn profile_fixture() -> Value {
    json!({"threads": [
        {"id":"1", "name":"hot", "running_samples":75, "heaviest_paths":[{"samples":75,"frames":["thread_start (in libsystem)","aft[123abc]::engine::derive (in ck-aft)","core[abc]::work"]}]},
        {"id":"2", "name":"noise", "running_samples":25, "heaviest_paths":[{"count":25,"frames":["_pthread_start","???"]}]}
    ]})
}
#[test]
fn perf_sweep_arguments_are_strict_and_bounded() {
    let args = arguments(&json!({"op":"perf.sweep"})).unwrap();
    assert_eq!(args.since_ms, 0);
    assert_eq!(args.profile_seconds, 15);
    for args in [
        json!({"profile_seconds":0}),
        json!({"profile_seconds":31}),
        json!({"profile_seconds":-1}),
        json!({"since_ms":-1}),
        json!({"since_ms":1.5}),
        json!({"pid":1}),
        json!({"path":"/etc/passwd"}),
    ] {
        assert_eq!(arguments(&args).unwrap_err().code, "perf_sweep_bad_request");
    }
    assert!(arguments(&json!({"since_ms":u64::MAX,"profile_seconds":30})).is_ok());
}
#[test]
fn perf_sweep_busy_refusal_and_release() {
    let running = AtomicBool::new(false);
    let first = acquire(&running).unwrap();
    let error = match acquire(&running) {
        Err(e) => e,
        Ok(_) => panic!("overlapping sweep admitted"),
    };
    assert_eq!(error.code, "busy");
    drop(first);
    assert!(
        acquire(&running).is_ok(),
        "completion must release the guard"
    );
}
#[test]
fn perf_sweep_success_shape_and_thread_rules() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, FIXTURE).unwrap();
    let report = collect(
        &Args {
            since_ms: SINCE,
            profile_seconds: 15,
        },
        &log,
        LOG_BYTES,
        LOG_LINES,
        || Ok(profile_fixture()),
    );
    assert_eq!(report["pid"], std::process::id());
    assert_eq!(report["card_sha256"].as_str().unwrap().len(), 64);
    #[cfg(unix)]
    {
        assert!(report["cpu_percent"].as_f64().is_some(), "{report}");
        assert!(report["rss_bytes"].as_u64().unwrap() > 0);
        assert!(report["uptime"].as_str().is_some());
        assert_eq!(report["load_average"].as_array().unwrap().len(), 3);
    }
    assert_eq!(report["since_ms"], SINCE);
    assert!(report["until_ms"].as_u64().unwrap() >= SINCE);
    assert_eq!(report["threads_total"], 2);
    assert_eq!(report["threads"][0]["share_percent"], 75.0);
    assert_eq!(
        report["threads"][0]["top_stacks"][0]["frames"],
        json!(["engine::derive", "work"])
    );
    assert_eq!(
        report["threads"][1]["top_stacks"][0]["frames"],
        json!(["_pthread_start", "???"])
    );
    assert_eq!(report["log"]["truncated"], false);
    assert_eq!(report["log"]["error"], 1);
    assert_eq!(report["log"]["panic"], 1);
}
#[test]
fn perf_sweep_profile_timeout_is_gap_not_lost_log_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, FIXTURE).unwrap();
    let report = collect(
        &Args {
            since_ms: SINCE,
            profile_seconds: 15,
        },
        &log,
        LOG_BYTES,
        LOG_LINES,
        || Err("profile_timeout: child exceeded its hard deadline".into()),
    );
    assert_eq!(report["threads"], json!([]));
    assert!(report["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|gap| gap["name"] == "profile_timeout"
            && gap["detail"]
                .as_str()
                .unwrap()
                .starts_with("profile_timeout:")));
    assert_eq!(report["log"]["error"], 1);
    assert_eq!(report["log"]["stats"]["slow_total"]["n"], 4);
}
#[test]
fn perf_sweep_log_caps_stop_the_iterator_and_report_truncation() {
    use std::cell::Cell;
    struct Counted<'a> {
        bytes: &'a [u8],
        read: &'a Cell<usize>,
    }
    impl Read for Counted<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.bytes.read(out)?;
            self.read.set(self.read.get() + n);
            Ok(n)
        }
    }
    let bytes = Cell::new(0);
    let source = vec![b'x'; 100_000];
    let report = parse_log(
        Counted {
            bytes: &source,
            read: &bytes,
        },
        0,
        u64::MAX,
        128,
        LOG_LINES,
    )
    .unwrap();
    assert_eq!(report["truncated"], true);
    assert!(
        bytes.get() <= 129,
        "unterminated line read {} bytes",
        bytes.get()
    );
    assert_eq!(report["lines_in_window"], 0);
    let source = "2025-01-01T00:00:01 WARN one\n".repeat(10_000);
    bytes.set(0);
    let report = parse_log(
        Counted {
            bytes: source.as_bytes(),
            read: &bytes,
        },
        0,
        u64::MAX,
        LOG_BYTES,
        2,
    )
    .unwrap();
    assert_eq!(report["truncated"], true);
    assert_eq!(report["scanned_lines"], 2);
    assert_eq!(report["warn"], 2);
    assert!(
        bytes.get() <= 8192,
        "line cap must stop before the rest of the source"
    );
    let exact = "2025-01-01T00:00:01 WARN one\n";
    assert_eq!(
        parse_log(
            exact.as_bytes(),
            SINCE + 1_000,
            u64::MAX,
            exact.len() as u64,
            1
        )
        .unwrap()["truncated"],
        false
    );
}

#[test]
fn perf_sweep_accepts_wall_and_cpu_phase_fields() {
    let log = "2025-01-01T00:00:01.000Z [aft] WARN slow tool_call name=edit total=50ms queue=3.123ms/cpu=? translate=39.123ms/cpu=0.004ms exec=7.123ms/cpu=2.345ms\n";
    let result = parse_log(log.as_bytes(), SINCE, u64::MAX, LOG_BYTES, LOG_LINES).unwrap();
    assert_eq!(result["stats"]["slow_total"]["n"], 1);
    assert_eq!(result["stats"]["slow_total"]["max"], 50);
    assert_eq!(result["stats"]["slow_queue"]["max"], 3);
    assert_eq!(result["stats"]["slow_exec"]["max"], 7);
}

#[test]
fn perf_sweep_log_windows_are_half_open_and_consecutive_windows_cover_each_entry_once() {
    let log = concat!(
        "2025-01-01T00:00:00.000Z [aft] WARN slow tool_call name=window total=10ms queue=1ms exec=9ms\n",
        "2025-01-01T00:00:01.000Z [aft] WARN slow tool_call name=window total=20ms queue=2ms exec=18ms\n",
        "2025-01-01T00:00:02.000Z [aft] WARN slow tool_call name=window total=30ms queue=3ms exec=27ms\n",
        "2025-01-01T00:00:03.000Z [aft] WARN slow tool_call name=window total=40ms queue=4ms exec=36ms\n",
        "2025-01-01T00:00:04.000Z [aft] WARN slow tool_call name=window total=50ms queue=5ms exec=45ms\n",
    );
    let first = parse_log(log.as_bytes(), SINCE, SINCE + 2_000, LOG_BYTES, LOG_LINES).unwrap();
    let second = parse_log(
        log.as_bytes(),
        SINCE + 2_000,
        SINCE + 4_000,
        LOG_BYTES,
        LOG_LINES,
    )
    .unwrap();

    assert_eq!(first["stats"]["slow_total"]["n"], 2);
    assert_eq!(first["stats"]["slow_total"]["max"], 20);
    assert_eq!(second["stats"]["slow_total"]["n"], 2);
    assert_eq!(second["stats"]["slow_total"]["max"], 40);
    assert_eq!(
        first["stats"]["slow_total"]["n"].as_u64().unwrap()
            + second["stats"]["slow_total"]["n"].as_u64().unwrap(),
        4
    );
}

#[test]
fn perf_sweep_reads_recent_entries_from_bounded_log_tail() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    let old_line = format!(
        "2024-12-31T23:59:59.000Z [aft] WARN slow tool_call name=old total=999ms queue=1ms exec=998ms {}\n",
        "x".repeat(LOG_BYTES as usize)
    );
    let recent = concat!(
        "2025-01-01T00:00:01.000Z [aft] WARN slow tool_call name=recent total=11ms queue=2ms exec=9ms\n",
        "2025-01-01T00:00:02.000Z [aft] WARN slow tool_call name=recent total=13ms queue=3ms exec=10ms\n",
    );
    std::fs::write(&log, format!("{old_line}{recent}")).unwrap();
    assert!(std::fs::metadata(&log).unwrap().len() > LOG_BYTES);

    let report = collect(
        &Args {
            since_ms: SINCE + 1_000,
            profile_seconds: 15,
        },
        &log,
        LOG_BYTES,
        LOG_LINES,
        || Ok(profile_fixture()),
    );
    assert_eq!(report["log"]["lines_in_window"], 2);
    assert_eq!(report["log"]["stats"]["slow_total"]["n"], 2);
    assert_eq!(report["log"]["slow_by_tool"][0]["name"], "recent");
    assert_eq!(report["log"]["slow_by_tool"][0]["count"], 2);
    assert_eq!(report["log"]["window_start"], "2025-01-01T00:00:01.000Z");
    assert_eq!(report["log"]["truncated"], false);

    let older = collect(
        &Args {
            since_ms: SINCE,
            profile_seconds: 15,
        },
        &log,
        LOG_BYTES,
        LOG_LINES,
        || Ok(profile_fixture()),
    );
    assert_eq!(older["log"]["truncated"], true);
    assert_eq!(older["log"]["window_start"], "2025-01-01T00:00:01.000Z");
}

#[test]
fn perf_sweep_parser_matches_script_python_on_shared_fixture() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/perf_sweep_oracle.py"
        ))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/perf_sweep.txt"
        ))
        .arg(iso_seconds(SINCE))
        .output()
        .expect("python3 required for parser parity");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut actual = parse_log(FIXTURE.as_bytes(), SINCE, u64::MAX, LOG_BYTES, LOG_LINES).unwrap();
    for key in [
        "scanned_lines",
        "scanned_bytes",
        "truncated",
        "window_start",
    ] {
        actual.as_object_mut().unwrap().remove(key);
    }
    assert_eq!(actual, expected);
    assert_eq!(
        actual["stats"]["slow_total"],
        json!({"n":4,"median":250.5,"p95":301,"max":999})
    );
    assert!(actual["warn_shapes"]
        .as_array()
        .unwrap()
        .contains(&json!({"count":2,"shape":"repeat N root=N"})));
}
#[cfg(unix)]
#[test]
fn perf_sweep_native_child_deadline_and_output_bound() {
    let start = Instant::now();
    let error = child_output(
        Path::new("/bin/sh"),
        &["-c", "printf 'sampler stalled' >&2; sleep 5"],
        start + Duration::from_millis(100),
        100,
    )
    .unwrap_err();
    assert!(error.starts_with("timeout:"), "{error}");
    assert!(error.contains("sampler stalled"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(2));
    let error = child_output(
        Path::new("/bin/sh"),
        &["-c", "printf 123456789"],
        Instant::now() + Duration::from_secs(2),
        8,
    )
    .unwrap_err();
    assert!(error.starts_with("output_limit:"), "{error}");
    assert_eq!(
        child_output(
            Path::new("/bin/sh"),
            &["-c", "printf 12345678"],
            Instant::now() + Duration::from_secs(2),
            8
        )
        .unwrap(),
        b"12345678"
    );
}
#[cfg(unix)]
#[test]
fn perf_sweep_child_stderr_tail_keeps_cause_and_stdout_separate() {
    let payload = format!(
        "discarded-head{}specific sampler failure",
        "x".repeat(STDERR_BYTES * 4)
    );
    let error = child_output(
        Path::new("/bin/sh"),
        &["-c", "printf '%s' \"$1\" >&2; exit 17", "sh", &payload],
        Instant::now() + Duration::from_secs(2),
        128,
    )
    .unwrap_err();
    assert!(error.starts_with("exit: child"), "{error}");
    assert!(error.contains("stderr tail (truncated)"), "{error}");
    assert!(error.ends_with("specific sampler failure"), "{error}");
    assert!(!error.contains("discarded-head"));
    assert!(error.len() <= STDERR_BYTES + 128);
    let bytes = child_output(
        Path::new("/bin/sh"),
        &[
            "-c",
            "printf 'debug artifact warning' >&2; printf '{\"threads\":[]}'",
        ],
        Instant::now() + Duration::from_secs(2),
        128,
    )
    .unwrap();
    assert_eq!(profile_result(Ok(bytes)).unwrap(), json!({"threads":[]}));
}

#[cfg(unix)]
#[test]
fn perf_sweep_symbolication_failure_is_named_gap_with_stderr_and_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, FIXTURE).unwrap();
    let report = collect(
        &Args {
            since_ms: SINCE,
            profile_seconds: 1,
        },
        &log,
        LOG_BYTES,
        LOG_LINES,
        || {
            profile_result(child_output(Path::new("/bin/sh"), &["-c", "printf 'could not find a DWARF/debug file; the unsymbolicated sample was kept at /tmp/sample.txt' >&2; exit 1"], Instant::now() + Duration::from_secs(2), 128))
        },
    );
    let gap = report["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "profile_symbolication_failed")
        .expect("symbolication failure must be a named gap");
    assert!(gap["detail"]
        .as_str()
        .unwrap()
        .contains("stderr tail:\ncould not find a DWARF/debug file"));
    assert_eq!(report["threads"], json!([]));
    assert_eq!(report["log"]["error"], 1);
    assert!(report["rss_bytes"].as_u64().unwrap() > 0);
}

#[cfg(target_os = "macos")]
fn host_executable() -> PathBuf {
    let supplied = PathBuf::from(
        std::env::var_os("AFT_PERF_TEST_EXE").expect("set AFT_PERF_TEST_EXE to the built aft CLI"),
    );
    std::fs::canonicalize(&supplied)
        .or_else(|_| {
            // cargo test normally starts in the member crate, but the documented
            // relative target path is rooted at the workspace that built the CLI.
            std::fs::canonicalize(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(&supplied),
            )
        })
        .expect("AFT_PERF_TEST_EXE must name a built aft CLI")
}

#[cfg(target_os = "macos")]
struct HostTarget(std::process::Child);
#[cfg(target_os = "macos")]
impl Drop for HostTarget {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[cfg(target_os = "macos")]
fn host_profile(exe: &Path, root: &Path, dsym: &Path) -> Result<Value, String> {
    use std::process::{Command, Stdio};
    // libtest cannot answer the profiler's --version probe. Keep a real CLI
    // waiting on stdin as the target, with an isolated working/storage directory.
    let target = HostTarget(
        Command::new(exe)
            .current_dir(root)
            .env("AFT_STORAGE_DIR", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (result, forks) = crate::effective_path::fork_counter::forks_during(|| {
        profile_result(child_output(
            exe,
            &[
                "profile",
                "--pid",
                &target.0.id().to_string(),
                "--seconds",
                "1",
                "--dsym",
                dsym.to_str().unwrap(),
                "--json",
            ],
            Instant::now() + Duration::from_secs(20),
            PROFILE_BYTES,
        ))
    });
    assert_eq!(forks, 0, "the daemon must never fork to sample itself");
    result
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a built aft CLI, dsymutil and macOS sampling permission"]
fn perf_sweep_macos_profile_child_no_daemon_fork() {
    let exe = host_executable();
    let dir = tempfile::tempdir().unwrap();
    let dsym = dir.path().join("aft.dSYM");
    let output = std::process::Command::new("/usr/bin/dsymutil")
        .arg(&exe)
        .arg("-o")
        .arg(&dsym)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "dsymutil: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profile =
        host_profile(&exe, dir.path(), &dsym).expect("real profile child must return JSON");
    assert!(!summarize_profile(profile).unwrap().0.is_empty());
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a built aft CLI and macOS sampling permission"]
fn perf_sweep_macos_profile_child_missing_dsym_is_named_gap() {
    let exe = host_executable();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, FIXTURE).unwrap();
    // An explicitly empty bundle bypasses staged caches and downloads, making
    // the missing-artifact case deterministic without altering production lookup.
    let missing = dir.path().join("empty.dSYM");
    std::fs::create_dir(&missing).unwrap();
    let report = collect(
        &Args {
            since_ms: SINCE,
            profile_seconds: 1,
        },
        &log,
        LOG_BYTES,
        LOG_LINES,
        || host_profile(&exe, dir.path(), &missing),
    );
    let gap = report["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "profile_symbolication_failed")
        .expect("missing dSYM must be reported as a named gap");
    let detail = gap["detail"].as_str().unwrap();
    assert!(detail.contains("stderr tail:"), "{detail}");
    assert!(
        detail.contains("could not find a DWARF/debug file"),
        "{detail}"
    );
    assert!(
        detail.contains("unsymbolicated sample was kept"),
        "{detail}"
    );
    assert_eq!(report["threads"], json!([]));
    assert_eq!(report["log"]["error"], 1);
    assert!(report["rss_bytes"].as_u64().unwrap() > 0);
}

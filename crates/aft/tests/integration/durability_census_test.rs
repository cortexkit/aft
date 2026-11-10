//! Durability census: counts the fsyncs, renames, links and writes the `aft`
//! daemon issues for one logical operation of each store, without changing any
//! store code. The daemon is started with a small syscall tracer
//! (`durability_census_interposer.c`) loaded through DYLD_INSERT_LIBRARIES; the
//! tracer appends one line per call to a trace file, and this test slices the
//! trace per operation and groups it by store directory.
//!
//! The syscall count gates run on macOS; portable unit tests also record real
//! syncs. The original ignored measurements remain available to compare counts:
//!
//! ```text
//! cargo test -p agent-file-tools --test integration -- \
//!     durability_census --ignored --nocapture --test-threads=1
//! ```
//!
//! Set AFT_DURABILITY_CENSUS_OUT to a directory to keep the raw trace and the
//! per-operation slices. macOS only: the tracer uses dyld interposition, and on
//! macOS Rust's `File::sync_all` is `fcntl(F_FULLFSYNC)`, which is the call the
//! census is about.
#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

const SESSION: &str = "durability-census";
const INTERPOSER_SOURCE: &str = include_str!("durability_census_interposer.c");
/// How long to keep tracing after a response so background threads (database
/// mirrors, watchdogs, lock release) finish the operation's I/O.
const SETTLE: Duration = Duration::from_millis(1500);

#[derive(Default, Clone)]
struct Tally {
    full_fsync_file: u64,
    full_fsync_dir: u64,
    barrier_fsync: u64,
    fsync: u64,
    fdatasync: u64,
    renames: u64,
    links: u64,
    writes: u64,
    write_bytes: u64,
    sync_ns: u64,
}

impl Tally {
    fn add(&mut self, event: &Event) {
        match event.op.as_str() {
            "F_FULLFSYNC" if event.kind == "dir" => self.full_fsync_dir += 1,
            "F_FULLFSYNC" => self.full_fsync_file += 1,
            "F_BARRIERFSYNC" => self.barrier_fsync += 1,
            "fsync" => self.fsync += 1,
            "fdatasync" => self.fdatasync += 1,
            "rename" => self.renames += 1,
            "link" => self.links += 1,
            "write" | "pwrite" | "writev" => {
                self.writes += 1;
                self.write_bytes += event.bytes;
            }
            _ => {}
        }
        if matches!(
            event.op.as_str(),
            "F_FULLFSYNC" | "F_BARRIERFSYNC" | "fsync" | "fdatasync"
        ) {
            self.sync_ns += event.duration_ns;
        }
    }

    fn syncs(&self) -> u64 {
        self.full_fsync_file
            + self.full_fsync_dir
            + self.barrier_fsync
            + self.fsync
            + self.fdatasync
    }
}

struct Event {
    pid: u32,
    op: String,
    kind: String,
    duration_ns: u64,
    bytes: u64,
    path: String,
    path2: String,
}

fn parse_trace(text: &str) -> Vec<Event> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 10 {
                return None;
            }
            Some(Event {
                pid: fields[0].parse().ok()?,
                op: fields[3].to_string(),
                kind: fields[4].to_string(),
                duration_ns: fields[5].parse().ok()?,
                bytes: fields[6].parse().unwrap_or(0),
                path: fields[8].to_string(),
                path2: fields[9].to_string(),
            })
        })
        .collect()
}

/// Group a path by store, relative to the storage directory or to the
/// daemon's private cache directory (where the pre-configure checkpoint lock
/// lives): `opencode/backups`, `index`, `aft.db-wal`, and so on. Paths outside
/// both are the project's user files or `other`.
fn store_of(path: &str, roots: &[(&str, &str)], project: &str) -> String {
    let path = path.strip_prefix("/private").unwrap_or(path);
    for (label, root) in roots {
        let Some(rest) = path.strip_prefix(root) else {
            continue;
        };
        let rest = rest.trim_start_matches('/');
        let mut parts = rest.split('/').filter(|part| !part.is_empty());
        let Some(first) = parts.next() else {
            return format!("{label}: (root dir)");
        };
        let first = match first.strip_prefix('.') {
            Some(hidden) => format!(".{}", hidden.split('.').next().unwrap_or(hidden)),
            None => first.to_string(),
        };
        if matches!(first.as_str(), "opencode" | "pi" | "aft") {
            if let Some(second) = parts.next() {
                return format!("{label}: {first}/{second}");
            }
        }
        return format!("{label}: {first}");
    }
    if path.starts_with(project) {
        return "project (user files)".to_string();
    }
    "other".to_string()
}

/// Make a path readable in the per-event listing: relative to its root, with
/// long digit and hex runs (pids, timestamps, hashes, random names) collapsed.
fn short_path(path: &str, roots: &[(&str, &str)], project: &str) -> String {
    let path = path.strip_prefix("/private").unwrap_or(path);
    let mut text = path.to_string();
    for (label, root) in roots {
        if let Some(rest) = path.strip_prefix(root) {
            text = format!("<{label}>{rest}");
            break;
        }
    }
    if let Some(rest) = path.strip_prefix(project) {
        text = format!("<project>{rest}");
    }
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 12 {
            out.push('#');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

struct Census {
    trace: PathBuf,
    storage: String,
    cache: String,
    project: String,
    offset: usize,
    report: String,
    out_dir: Option<PathBuf>,
    daemon_pid: u32,
    verify_counts: bool,
}

impl Census {
    fn mark(&mut self) {
        self.offset = std::fs::read(&self.trace).map(|b| b.len()).unwrap_or(0);
    }

    fn record(&mut self, label: &str, elapsed: Duration) {
        let bytes = std::fs::read(&self.trace).unwrap_or_default();
        let slice = String::from_utf8_lossy(&bytes[self.offset.min(bytes.len())..]).into_owned();
        self.offset = bytes.len();
        if let Some(dir) = &self.out_dir {
            let name: String = label
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            std::fs::write(dir.join(format!("{name}.trace")), &slice).unwrap();
        }
        let events = parse_trace(&slice);
        let roots = [
            ("storage", self.storage.as_str()),
            ("cache", self.cache.as_str()),
        ];
        summarize(
            &mut self.report,
            label,
            &format!("response {:.1} ms", elapsed.as_secs_f64() * 1000.0),
            &events,
            &roots,
            &self.project,
            self.daemon_pid,
        );
        if self.verify_counts {
            if label.starts_with("configure + startup") {
                // SQLite keeps its own fsync policy. These stores' extra drive
                // flushes are different calls and must not creep back in.
                for store in [
                    "storage: index",
                    "storage: symbols",
                    "storage: callgraph",
                    "storage: inspect",
                    "storage: artifact-owners",
                ] {
                    let count = events
                        .iter()
                        .filter(|event| {
                            store_of(&event.path, &roots, &self.project) == store
                                && matches!(event.op.as_str(), "F_FULLFSYNC" | "F_BARRIERFSYNC")
                        })
                        .count();
                    assert_eq!(count, 0, "{label}: {store} extra drive flushes");
                }
                return;
            }
            let (store, expected) = if label.starts_with("bash ") {
                ("storage: opencode/bash-tasks", 0)
            } else if label.starts_with("edit 1:") {
                // Three directory flushes for the new backup folders, then the
                // snapshot: its content file takes a full flush, the undo index
                // file and its folder take ordinary fsyncs, and meta.json's
                // full flush then commits all of them to the drive.
                self.assert_full_flushes(&events, &roots, label, 5);
                ("storage: opencode/backups", 7)
            } else if label.starts_with("edit ") {
                // The snapshot alone: two full flushes (content, meta.json)
                // and two ordinary fsyncs ordered before the last one.
                self.assert_full_flushes(&events, &roots, label, 2);
                ("storage: opencode/backups", 4)
            } else if label.starts_with("undo ") {
                // Include the user-file sync, not just stack metadata. The
                // restored file and meta.json take full flushes; the undo
                // index file and its folder take ordinary fsyncs that
                // meta.json's full flush commits.
                let in_scope = |event: &Event| {
                    let store = store_of(&event.path, &roots, &self.project);
                    store == "storage: opencode/backups" || store == "project (user files)"
                };
                let count = count_syncs(&events, in_scope);
                assert_eq!(count, 4, "{label}: undo syncs including restored file");
                let full = events
                    .iter()
                    .filter(|event| in_scope(event) && event.op == "F_FULLFSYNC")
                    .count();
                assert_eq!(full, 2, "{label}: undo full drive flushes");
                return;
            } else if label.starts_with("checkpoint create") {
                ("storage: opencode/checkpoints", 8)
            } else if label.starts_with("checkpoint ") {
                ("storage: opencode/checkpoints", 0)
            } else {
                return;
            };
            let count = count_syncs(&events, |event| {
                let actual = store_of(&event.path, &roots, &self.project);
                actual == store
                    || ((label.starts_with("edit 1:") || label.starts_with("checkpoint create"))
                        && actual == "storage: opencode")
            });
            assert_eq!(count, expected, "{label}: {store} sync count");
        }
    }
}

impl Census {
    /// Full drive flushes (`F_FULLFSYNC`) an edit makes under the backup store:
    /// the expensive kind on macOS, which an edit must not add back.
    fn assert_full_flushes(
        &self,
        events: &[Event],
        roots: &[(&str, &str)],
        label: &str,
        expected: u64,
    ) {
        let full = events
            .iter()
            .filter(|event| {
                let store = store_of(&event.path, roots, &self.project);
                (store == "storage: opencode/backups"
                    || (label.starts_with("edit 1:") && store == "storage: opencode"))
                    && event.op == "F_FULLFSYNC"
            })
            .count() as u64;
        assert_eq!(full, expected, "{label}: backup full drive flushes");
    }
}

fn count_syncs(events: &[Event], include: impl Fn(&Event) -> bool) -> u64 {
    events
        .iter()
        .filter(|event| {
            include(event)
                && matches!(
                    event.op.as_str(),
                    "F_FULLFSYNC" | "F_BARRIERFSYNC" | "fsync" | "fdatasync"
                )
        })
        .count() as u64
}

/// Append one operation's per-store table and its ordered list of syncs,
/// renames and links to `report`.
fn summarize(
    report: &mut String,
    label: &str,
    timing: &str,
    events: &[Event],
    roots: &[(&str, &str)],
    project: &str,
    expected_pid: u32,
) {
    let mut by_store: BTreeMap<String, Tally> = BTreeMap::new();
    let mut total = Tally::default();
    let mut other_pids = 0;
    let mut listing = String::new();
    for event in events {
        if event.pid != expected_pid {
            other_pids += 1;
        }
        // A rename or link is attributed to its destination's store.
        let path = if event.path2.is_empty() {
            &event.path
        } else {
            &event.path2
        };
        let store = store_of(path, roots, project);
        by_store.entry(store).or_default().add(event);
        total.add(event);
        if !matches!(event.op.as_str(), "write" | "pwrite" | "writev") {
            let _ = write!(
                listing,
                "    {:<12} {:<5} {:>7.2} ms  {}",
                event.op,
                event.kind,
                event.duration_ns as f64 / 1e6,
                short_path(&event.path, roots, project)
            );
            if !event.path2.is_empty() {
                let _ = write!(listing, " -> {}", short_path(&event.path2, roots, project));
            }
            listing.push('\n');
        }
    }
    let _ = writeln!(
        report,
        "\n## {label}  ({timing}; traced syncs {} taking {:.2} ms; events from other pids: {other_pids})",
        total.syncs(),
        total.sync_ns as f64 / 1e6,
    );
    let _ = writeln!(
        report,
        "{:<34} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>5} {:>7} {:>9} {:>9}",
        "store",
        "FFfile",
        "FFdir",
        "barr",
        "fsync",
        "fdsync",
        "rename",
        "link",
        "writes",
        "bytes",
        "sync_ms"
    );
    for (store, tally) in by_store {
        let _ = writeln!(
            report,
            "{:<34} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>5} {:>7} {:>9} {:>9.2}",
            store,
            tally.full_fsync_file,
            tally.full_fsync_dir,
            tally.barrier_fsync,
            tally.fsync,
            tally.fdatasync,
            tally.renames,
            tally.links,
            tally.writes,
            tally.write_bytes,
            tally.sync_ns as f64 / 1e6
        );
    }
    if !listing.is_empty() {
        let _ = writeln!(report, "  syncs, renames and links in order:");
        report.push_str(&listing);
    }
}

fn build_interposer(dir: &Path) -> PathBuf {
    let source = dir.join("interposer.c");
    let library = dir.join("interposer.dylib");
    std::fs::write(&source, INTERPOSER_SOURCE).unwrap();
    let status = Command::new("cc")
        .args(["-dynamiclib", "-O2", "-o"])
        .arg(&library)
        .arg(&source)
        .status()
        .expect("run cc");
    assert!(status.success(), "building the census interposer failed");
    library
}

fn send(aft: &mut AftProcess, request: Value) -> (Value, Duration) {
    let started = Instant::now();
    let response = aft.send(&request.to_string());
    (response, started.elapsed())
}

fn wait_terminal(aft: &mut AftProcess, task_id: &str) -> Value {
    let started = Instant::now();
    loop {
        let (status, _) = send(
            aft,
            json!({
                "id": "census-status",
                "session_id": SESSION,
                "command": "bash_status",
                "params": { "task_id": task_id },
            }),
        );
        if matches!(
            status["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out")
        ) {
            return status;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "{status:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run every measured operation once against one daemon. With `trace` set the
/// daemon carries the tracer and the report lists per-store counts; without it
/// only response times are reported, as an uninstrumented baseline.
fn run_scenario(trace: bool, verify_counts: bool) -> String {
    let scratch = tempfile::tempdir().unwrap();
    let project_dir = tempfile::tempdir().unwrap();
    let storage_dir = tempfile::tempdir().unwrap();
    let project = std::fs::canonicalize(project_dir.path()).unwrap();
    let storage = std::fs::canonicalize(storage_dir.path()).unwrap();
    // F_GETPATH reports /private/var/...; compare without the prefix.
    let unprivate = |path: &Path| {
        let text = path.display().to_string();
        text.strip_prefix("/private").unwrap_or(&text).to_string()
    };
    for index in 0..20 {
        std::fs::write(
            project.join(format!("module_{index}.rs")),
            format!("pub fn function_{index}() -> u32 {{ {index} }}\n"),
        )
        .unwrap();
    }
    let target = project.join("edited.txt");
    std::fs::write(&target, "original content\n").unwrap();

    let trace_path = scratch.path().join("trace.tsv");
    let library = build_interposer(scratch.path());
    let mut envs: Vec<(&str, &OsStr)> = Vec::new();
    if trace {
        envs.push(("DYLD_INSERT_LIBRARIES", library.as_os_str()));
        envs.push(("AFT_SYNC_TRACE", trace_path.as_os_str()));
    }
    let mut aft = AftProcess::spawn_with_env(&envs);
    let out_dir = std::env::var_os("AFT_DURABILITY_CENSUS_OUT").map(|dir| {
        let dir = PathBuf::from(dir).join(if trace { "traced" } else { "untraced" });
        std::fs::create_dir_all(&dir).unwrap();
        dir
    });
    let mut census = Census {
        trace: trace_path.clone(),
        storage: unprivate(&storage),
        cache: std::fs::canonicalize(aft.cache_dir())
            .unwrap()
            .display()
            .to_string()
            .trim_start_matches("/private")
            .to_string(),
        project: unprivate(&project),
        offset: 0,
        report: String::new(),
        out_dir,
        daemon_pid: aft.pid(),
        verify_counts,
    };
    let _ = writeln!(
        census.report,
        "# durability census ({})\nstorage={}\nproject={}",
        if trace { "traced" } else { "untraced" },
        census.storage,
        census.project
    );

    // Configure and let the startup work (index build, cache write) finish.
    census.mark();
    let (configured, elapsed) = send(
        &mut aft,
        json!({
            "id": "census-configure",
            "session_id": SESSION,
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "config": user_config(json!({ "bash": { "rewrite": false, "compress": false, "background": true } })),
        }),
    );
    assert_eq!(configured["success"], true, "{configured:?}");
    let _ = send(
        &mut aft,
        json!({ "id": "census-grep", "session_id": SESSION, "command": "grep", "pattern": "function_1", "path": project }),
    );
    std::thread::sleep(Duration::from_secs(4));
    census.record("configure + startup (index build and cache write)", elapsed);

    // Idle window: background activity the per-operation slices also contain.
    census.mark();
    std::thread::sleep(SETTLE);
    census.record("idle window (noise floor, same length as settle)", SETTLE);

    // One foreground bash command, to its terminal state. Repeated so the
    // counts are seen to be per command, not first-use costs.
    for round in 1..=3 {
        census.mark();
        let started = Instant::now();
        let (bash, _) = send(
            &mut aft,
            json!({ "id": "census-fg", "session_id": SESSION, "command": "bash", "params": { "command": "printf hi" } }),
        );
        assert_eq!(bash["success"], true, "{bash:?}");
        if bash["status"] != "completed" {
            wait_terminal(&mut aft, bash["task_id"].as_str().unwrap());
        }
        let elapsed = started.elapsed();
        std::thread::sleep(SETTLE);
        census.record(
            &format!("bash foreground `printf hi` #{round} (spawn to terminal)"),
            elapsed,
        );
    }

    // One background bash: the spawn reply, then the finish.
    census.mark();
    let (bash, elapsed) = send(
        &mut aft,
        json!({ "id": "census-bg", "session_id": SESSION, "command": "bash", "params": { "command": "sleep 2; printf done", "background": true } }),
    );
    assert_eq!(bash["success"], true, "{bash:?}");
    let task_id = bash["task_id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(1000));
    census.record("bash background spawn (reply + 1 s)", elapsed);
    census.mark();
    let started = Instant::now();
    wait_terminal(&mut aft, &task_id);
    let _ = send(
        &mut aft,
        json!({ "id": "census-drain", "session_id": SESSION, "command": "bash_drain_completions" }),
    );
    let elapsed = started.elapsed();
    std::thread::sleep(SETTLE);
    census.record("bash background finish (exit, status, drain)", elapsed);

    // One edit through `write` (backs up the old content), then its undo.
    census.mark();
    let (write, elapsed) = send(
        &mut aft,
        json!({ "id": "census-write", "session_id": SESSION, "command": "write", "file": target, "content": "second content\n" }),
    );
    assert_eq!(write["success"], true, "{write:?}");
    std::thread::sleep(SETTLE);
    census.record(
        "edit 1: write on an existing file (first backup of the file)",
        elapsed,
    );

    for (round, (from, to)) in [
        ("second", "third"),
        ("third", "fourth"),
        ("fourth", "fifth"),
    ]
    .into_iter()
    .enumerate()
    {
        census.mark();
        let (edit, elapsed) = send(
            &mut aft,
            json!({ "id": "census-edit", "session_id": SESSION, "command": "edit_match", "file": target, "match": from, "replacement": to }),
        );
        assert_eq!(edit["success"], true, "{edit:?}");
        std::thread::sleep(SETTLE);
        census.record(
            &format!(
                "edit {}: edit_match on the same file (stack depth {})",
                round + 2,
                round + 2
            ),
            elapsed,
        );
    }

    census.mark();
    let (undo, elapsed) = send(
        &mut aft,
        json!({ "id": "census-undo", "session_id": SESSION, "command": "undo", "file": target }),
    );
    assert_eq!(undo["success"], true, "{undo:?}");
    std::thread::sleep(SETTLE);
    census.record("undo (one step)", elapsed);

    // Checkpoint create / list / restore over three files.
    let files: Vec<String> = (0..3)
        .map(|index| {
            project
                .join(format!("module_{index}.rs"))
                .display()
                .to_string()
        })
        .collect();
    census.mark();
    let (checkpoint, elapsed) = send(
        &mut aft,
        json!({ "id": "census-cp", "session_id": SESSION, "command": "checkpoint", "name": "census", "files": files }),
    );
    assert_eq!(checkpoint["success"], true, "{checkpoint:?}");
    std::thread::sleep(SETTLE);
    census.record("checkpoint create (3 files)", elapsed);

    for round in 1..=2 {
        census.mark();
        let (list, elapsed) = send(
            &mut aft,
            json!({ "id": "census-list", "session_id": SESSION, "command": "list_checkpoints" }),
        );
        assert_eq!(list["success"], true, "{list:?}");
        std::thread::sleep(SETTLE);
        census.record(&format!("checkpoint list #{round}"), elapsed);
    }

    std::fs::write(project.join("module_0.rs"), "changed\n").unwrap();
    census.mark();
    let (restore, elapsed) = send(
        &mut aft,
        json!({ "id": "census-restore", "session_id": SESSION, "command": "restore_checkpoint", "name": "census" }),
    );
    assert_eq!(restore["success"], true, "{restore:?}");
    std::thread::sleep(SETTLE);
    census.record("checkpoint restore (3 files)", elapsed);

    census.mark();
    let started = Instant::now();
    let status = aft.shutdown();
    let elapsed = started.elapsed();
    census.record("shutdown", elapsed);
    assert!(status.success());

    if let Some(dir) = std::env::var_os("AFT_DURABILITY_CENSUS_OUT") {
        let dir = PathBuf::from(dir).join(if trace { "traced" } else { "untraced" });
        if trace {
            std::fs::copy(&trace_path, dir.join("full.trace")).unwrap();
        }
        std::fs::write(dir.join("report.txt"), &census.report).unwrap();
    }
    census.report
}

#[test]
#[ignore = "measurement: counts fsyncs per store operation; run alone with --nocapture"]
fn durability_census_per_operation() {
    let traced = run_scenario(true, false);
    println!("{traced}");
    let untraced = run_scenario(false, false);
    println!("{untraced}");
}

#[test]
fn durability_syscall_operation_counts() {
    println!("{}", run_scenario(true, true));
}

/// Cost of one sync of a small freshly written file on this disk, for each
/// flavour macOS offers. Rust's `File::sync_all` and `sync_data` are both
/// `F_FULLFSYNC` on Apple targets.
#[test]
#[ignore = "measurement: per-call sync cost on this disk; run alone with --nocapture"]
fn durability_census_sync_flavour_costs() {
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().unwrap();
    let rounds = 40;
    let mut report = String::new();
    for (name, flavour) in [
        ("F_FULLFSYNC (File::sync_all)", 0),
        ("F_BARRIERFSYNC", 1),
        ("fsync(2)", 2),
        ("no sync", 3),
    ] {
        let mut file_ns = Vec::new();
        let mut dir_ns = Vec::new();
        for round in 0..rounds {
            let path = dir.path().join(format!("{flavour}-{round}.tmp"));
            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(&[b'x'; 4096]).unwrap();
            let sync = |fd: i32| unsafe {
                match flavour {
                    0 => libc::fcntl(fd, libc::F_FULLFSYNC),
                    1 => libc::fcntl(fd, libc::F_BARRIERFSYNC),
                    2 => libc::fsync(fd),
                    _ => 0,
                }
            };
            let started = Instant::now();
            assert_eq!(sync(file.as_raw_fd()), 0);
            file_ns.push(started.elapsed().as_nanos() as u64);
            std::fs::rename(&path, dir.path().join(format!("{flavour}-{round}.final"))).unwrap();
            let handle = std::fs::File::open(dir.path()).unwrap();
            let started = Instant::now();
            assert_eq!(sync(handle.as_raw_fd()), 0);
            dir_ns.push(started.elapsed().as_nanos() as u64);
        }
        file_ns.sort_unstable();
        dir_ns.sort_unstable();
        let _ = writeln!(
            report,
            "{name:<30} file p50 {:>7.3} ms p90 {:>7.3} ms | dir p50 {:>7.3} ms p90 {:>7.3} ms",
            file_ns[rounds / 2] as f64 / 1e6,
            file_ns[rounds * 9 / 10] as f64 / 1e6,
            dir_ns[rounds / 2] as f64 / 1e6,
            dir_ns[rounds * 9 / 10] as f64 / 1e6,
        );
    }
    println!("{report}");
}

//! Reproductions for the AFT daemon whose macOS `phys_footprint` reached
//! ~9 GB, almost all dirty "Malloc Small". The investigation is written up in
//! `docs/investigations/daemon-malloc-small-growth-2026-09.md`.
//!
//! What the live daemon showed: libmalloc's own `bytes_in_use` was 8.5-8.9 GB,
//! about equal to its footprint, while the memory census attributed only
//! ~3.1 GB to roots. The memory is live, not freed-and-retained, and most of it
//! is invisible to the census.
//!
//! `parked_semantic_loads_are_unattributed` is the reproduction of the cause
//! found. It runs the configured `ck-subc` bytes from the shared immutable test
//! executable cache in a temporary, isolated home, with the `aft` binary this
//! package builds as its module, and opens one short session per root: bind, run
//! a 15 s command, close the route, the way agent sessions come and go. Each bind runs configure, which reloads that root's
//! semantic index from disk on a background thread, and the next completion
//! drain installs it. The name reflects the first hypothesis (a finished load
//! parked in its channel); that turned out not to hold, because unbinding a
//! root drops the receiver and with it any undelivered load. The real cause:
//! on install the drain freezes an owner root's index into a shared base, a
//! view of a shared base reports zero bytes on its root's row, and the
//! process-wide shared-base total counted only bases registered by borrowed
//! loads, so the owner's index was counted nowhere until the idle-TTL reaper
//! dropped it. The assertion states the property the daemon should have (live
//! index bytes are attributed). The deterministic in-process version of this
//! check is
//! `runtime_drain::tests::semantic_index_installed_by_the_drain_is_attributed_by_the_census`.
//!
//! The two `control_*` tests drive the other workloads the daemon log showed
//! most often, in-process, and pass: cold search-index rebuilds after watcher
//! overflow and repeated semantic loads do not retain memory once their result
//! is dropped. They are kept so the negative result can be re-checked.
//!
//! All tests are `#[ignore]`d: they are slow, macOS-only measurements. Run
//! with, for example:
//!
//! ```text
//! AFT_REPRO_SUBC_BIN=~/.local/share/cortexkit/bin/ck-subc \
//! AFT_REPRO_SUBC_PROBE=~/.local/share/cortexkit/bin/subc-probe \
//! cargo test -p agent-file-tools --test daemon_malloc_small_growth_repro -- \
//!     --ignored --nocapture --test-threads=1
//! ```
//!
//! The daemon test never touches the operator's daemon: it starts its own
//! `ckdev-subc` copy with every XDG directory, `HOME` and `TMPDIR` pointed into a
//! temporary directory, on a kernel-assigned port, in its own process group,
//! and kills that group when it finishes. The daemon process uses the shared
//! content-addressed executable cache rather than creating a fresh copy in each
//! test home. It runs the module with
//! `MallocStackLogging=1` and reads that process with `malloc_history`, which
//! briefly suspends it; that is safe only because the process is the test's own.

#![cfg(target_os = "macos")]

#[path = "helpers/executable_cache.rs"]
mod executable_cache;

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aft::semantic_index::SemanticIndex;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Counts live bytes exactly so the in-process controls do not depend on the
/// allocator's own bookkeeping, which is part of what is under suspicion.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(pointer, layout);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = System.realloc(pointer, layout, new_size);
        if !new_pointer.is_null() {
            LIVE_BYTES.fetch_add(new_size, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        new_pointer
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const MB: f64 = 1024.0 * 1024.0;
/// Width of the stub's vectors. 1024 is the width of qwen3-embedding-0.6b, the
/// model the production daemon under investigation uses, so per-entry sizes
/// are comparable.
const DIMENSION: usize = 1024;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Corpus and embedding stub shared by every scenario.
// ---------------------------------------------------------------------------

/// Deterministic pseudo-random identifiers so trigram and chunk counts are
/// realistic (many distinct trigrams) but reproducible across runs.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn ident(&mut self) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz_0123456789";
        let len = 6 + (self.next() % 10) as usize;
        let mut out = String::with_capacity(len + 1);
        out.push((b'a' + (self.next() % 26) as u8) as char);
        for _ in 0..len {
            out.push(ALPHABET[(self.next() % ALPHABET.len() as u64) as usize] as char);
        }
        out
    }
}

/// Rust sources with `functions` small documented functions each, spread over
/// nested directories the way a real repository is.
fn write_corpus(root: &Path, files: usize, functions: usize, seed: u64) -> Vec<PathBuf> {
    let mut rng = Lcg(seed);
    let mut paths = Vec::with_capacity(files);
    for file in 0..files {
        let dir = root
            .join(format!("crate_{}", file % 37))
            .join(format!("module_{}", file % 11));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("file_{file}.rs"));
        let mut text = String::new();
        for _ in 0..functions {
            let name = rng.ident();
            let arg = rng.ident();
            let other = rng.ident();
            writeln!(
                text,
                "/// Computes {name} from {arg} using {other}.\npub fn {name}({arg}: u64) -> u64 {{\n    let {other} = {arg}.wrapping_mul({});\n    {other} ^ {}\n}}\n",
                rng.next(),
                rng.next()
            )
            .unwrap();
        }
        std::fs::write(&path, text).unwrap();
        paths.push(path);
    }
    paths
}

/// A loopback OpenAI-compatible embedding backend that answers every row with
/// the same vector. Model-list probes (requests without a body) get a
/// one-model list so the backend health probe passes.
struct StubEmbeddingServer {
    base_url: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StubEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub embedding server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("stub address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let connection_shutdown = Arc::clone(&thread_shutdown);
                        thread::spawn(move || serve_connection(stream, &connection_shutdown));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Self {
            base_url: format!("http://{address}/v1"),
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for StubEmbeddingServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_connection(mut stream: TcpStream, shutdown: &AtomicBool) {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut pending = Vec::new();
    let mut chunk = [0u8; 65536];
    while !shutdown.load(Ordering::SeqCst) {
        let count = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        pending.extend_from_slice(&chunk[..count]);
        while let Some(position) = pending.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = position + 4;
            let mut content_length = 0usize;
            for line in String::from_utf8_lossy(&pending[..header_end]).lines() {
                if line.to_ascii_lowercase().starts_with("content-length:") {
                    content_length = line
                        .split_once(':')
                        .and_then(|(_, value)| value.trim().parse().ok())
                        .unwrap_or(0);
                }
            }
            if pending.len() < header_end + content_length {
                break;
            }
            let payload = if content_length == 0 {
                serde_json::json!({"object": "list", "data": [{"id": "stub-embedding", "object": "model"}]})
                    .to_string()
            } else {
                let body: serde_json::Value =
                    serde_json::from_slice(&pending[header_end..header_end + content_length])
                        .unwrap_or(serde_json::Value::Null);
                let rows = body["input"].as_array().map(Vec::len).unwrap_or(1);
                let vector = vec![0.125f32; DIMENSION];
                let data = (0..rows)
                    .map(|index| serde_json::json!({"object": "embedding", "embedding": vector, "index": index}))
                    .collect::<Vec<_>>();
                serde_json::json!({"object": "list", "data": data}).to_string()
            };
            pending.drain(..header_end + content_length);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload,
            );
            if stream.write_all(response.as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Reproduction: a hermetic subc daemon with the aft module.
// ---------------------------------------------------------------------------

/// A `ckdev-subc` daemon started in its own process group under an isolated home.
/// Dropping it kills the whole group, which includes the aft module process
/// the daemon spawned.
struct HermeticDaemon {
    child: Child,
    connection_file: PathBuf,
}

impl HermeticDaemon {
    fn start(subc_bin: &Path, home: &Path) -> Self {
        let runtime = home.join("runtime");
        let connection_file = runtime.join("subc-connection.json");
        let _ = std::fs::remove_file(&connection_file);
        let test_bin = cache_test_daemon(subc_bin);
        let child = Command::new(&test_bin)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home.join("home"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("TMPDIR", home.join("tmp"))
            .env("SUBC_PORT", "0")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start hermetic ckdev-subc");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !connection_file.exists() {
            assert!(
                Instant::now() < deadline,
                "hermetic ckdev-subc did not publish its connection file"
            );
            thread::sleep(Duration::from_millis(200));
        }
        // The daemon writes its connection file before it has spawned the aft
        // module and accepted its attach; give the module time to connect.
        thread::sleep(Duration::from_secs(3));
        Self {
            child,
            connection_file,
        }
    }
}

fn cache_test_daemon(subc_bin: &Path) -> PathBuf {
    let bytes = std::fs::read(subc_bin).expect("read daemon for the executable cache");
    executable_cache::cached_executable_named(&bytes, "ckdev-subc")
        .expect("cache immutable test daemon")
}

#[test]
fn cached_test_daemon_keeps_ckdev_name() {
    let script = b"#!/bin/sh\nprintf '%s\\n' \"$0\"\n";
    let first = executable_cache::cached_executable_named(script, "ckdev-subc").unwrap();
    let second = executable_cache::cached_executable_named(script, "ckdev-subc").unwrap();
    assert_eq!(first, second);
    assert_eq!(first.file_name(), Some(std::ffi::OsStr::new("ckdev-subc")));
    assert_eq!(first.parent().unwrap().file_name().unwrap().len(), 64);
    let output = Command::new(&first)
        .output()
        .expect("run cached daemon fixture");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        first.to_string_lossy()
    );
}

impl Drop for HermeticDaemon {
    fn drop(&mut self) {
        // The daemon was started with process_group(0), so its pid is the
        // group id and killing the group also stops the aft module it spawned.
        unsafe {
            libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

/// Bind `root` as `session`, call one tool, and close the route (the probe
/// exits), which is how a short agent session looks to the daemon.
fn probe(
    probe_bin: &Path,
    daemon: &HermeticDaemon,
    root: &Path,
    session: &str,
    tool: &str,
    args: &str,
) -> String {
    let output = Command::new(probe_bin)
        .arg("--subc")
        .arg(&daemon.connection_file)
        .args([
            "--module-id",
            "aft",
            "--harness",
            "runner",
            "--session",
            session,
        ])
        .arg("--root")
        .arg(root)
        .args(["--tool", tool, "--args", args])
        .stdin(Stdio::null())
        .output()
        .expect("run subc-probe");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// First integer value for `key` in the probe's JSON output. The residual
/// `unattributed_bytes` is signed, so a leading minus is accepted and a
/// negative value reads as zero.
fn json_u64(text: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let start = text.find(&needle)? + needle.len();
    let rest = text[start..].trim_start();
    let digits: String = rest
        .chars()
        .enumerate()
        .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && *c == '-'))
        .map(|(_, c)| c)
        .collect();
    let value: i64 = digits.parse().ok()?;
    Some(value.max(0) as u64)
}

#[derive(Debug, Clone, Copy)]
struct Census {
    footprint: u64,
    attributed: u64,
    unattributed: u64,
}

fn census(probe_bin: &Path, daemon: &HermeticDaemon, meter_root: &Path) -> Census {
    let text = probe(probe_bin, daemon, meter_root, "meter", "status", "{}");
    Census {
        footprint: json_u64(&text, "phys_footprint_bytes").expect("status phys_footprint_bytes"),
        attributed: json_u64(&text, "total_attributed_bytes")
            .expect("status total_attributed_bytes"),
        unattributed: json_u64(&text, "unattributed_bytes").expect("status unattributed_bytes"),
    }
}

fn semantic_artifacts(home: &Path) -> usize {
    let dir = home.join("data/cortexkit/aft/semantic");
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().join("semantic.bin").exists())
                .count()
        })
        .unwrap_or(0)
}

/// Entry counts from every `loaded semantic index from disk: N entries` line
/// the module logged, i.e. every full index the daemon materialized from disk.
fn logged_semantic_loads(home: &Path) -> Vec<u64> {
    let mut counts = Vec::new();
    let Ok(entries) = std::fs::read_dir(home.join("data/cortexkit/aft/logs")) else {
        return counts;
    };
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in text.lines() {
            if let Some(rest) = line.split("loaded semantic index from disk: ").nth(1) {
                if let Some(count) = rest.split(' ').next().and_then(|n| n.parse().ok()) {
                    counts.push(count);
                }
            }
        }
    }
    counts
}

/// The aft module process of the hermetic daemon: the one whose arguments name
/// this test's private connection file.
fn module_pid(home: &Path) -> Option<u32> {
    let connection = home.join("runtime/subc-connection.json");
    let output = Command::new("/usr/bin/pgrep")
        .arg("-f")
        .arg(connection.as_os_str())
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .find(|pid| {
            Command::new("/bin/ps")
                .args(["-o", "command=", "-p", &pid.to_string()])
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_BIN_EXE_aft")))
                .unwrap_or(false)
        })
}

/// Sum of live bytes whose allocation stack contains `frame`, from
/// `malloc_history -allBySize`. The target is this test's own daemon.
fn live_bytes_under(pid: u32, frame: &str) -> u64 {
    let output = Command::new("/usr/bin/malloc_history")
        .arg(pid.to_string())
        .arg("-allBySize")
        .output()
        .expect("run malloc_history");
    let mut total = 0u64;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Lines read "<n> calls for <bytes> bytes: <frame> | <frame> | ...".
        let Some((head, stack)) = line.split_once(" bytes: ") else {
            continue;
        };
        if !stack.contains(frame) {
            continue;
        }
        if let Some(bytes) = head
            .split(" calls for ")
            .nth(1)
            .and_then(|b| b.trim().parse::<u64>().ok())
        {
            total += bytes;
        }
    }
    total
}

fn row(label: &str, census: Census) {
    eprintln!(
        "{label:<34} footprint={:>8.1}MB attributed={:>8.1}MB unattributed={:>8.1}MB",
        census.footprint as f64 / MB,
        census.attributed as f64 / MB,
        census.unattributed as f64 / MB,
    );
}

#[test]
#[ignore = "slow macOS daemon reproduction; needs AFT_REPRO_SUBC_BIN and AFT_REPRO_SUBC_PROBE"]
fn parked_semantic_loads_are_unattributed() {
    let subc_bin = PathBuf::from(
        std::env::var_os("AFT_REPRO_SUBC_BIN").expect("set AFT_REPRO_SUBC_BIN to a ck-subc binary"),
    );
    let probe_bin = PathBuf::from(
        std::env::var_os("AFT_REPRO_SUBC_PROBE")
            .expect("set AFT_REPRO_SUBC_PROBE to a subc-probe binary"),
    );
    let root_count = env_usize("AFT_REPRO_ROOTS", 6);
    let files_per_root = env_usize("AFT_REPRO_FILES", 2700);

    let stub = StubEmbeddingServer::start();
    let temp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(temp.path()).unwrap();
    if std::env::var_os("AFT_REPRO_KEEP_HOME").is_some() {
        // Keep the daemon's home (logs, artifacts) for inspection after the run.
        eprintln!("keeping hermetic home at {}", home.display());
        std::mem::forget(temp);
    }
    for dir in [
        "runtime",
        "config/cortexkit",
        "data",
        "state",
        "cache",
        "tmp",
        "home",
    ] {
        std::fs::create_dir_all(home.join(dir)).unwrap();
    }
    std::fs::write(
        home.join("config/cortexkit/subc.jsonc"),
        format!(
            r#"{{"version": 1,
  "storage": {{"backend": "sqlite", "data_home": "{data}"}},
  "modules": {{"aft": {{"program": "{aft}", "args": [],
              "env": {{"MallocStackLogging": "1"}}, "enabled": true}}}}}}"#,
            data = home.join("data").display(),
            aft = env!("CARGO_BIN_EXE_aft"),
        ),
    )
    .unwrap();
    std::fs::write(
        home.join("config/cortexkit/aft.jsonc"),
        format!(
            r#"{{"indexes": {{"trigram": true, "semantic": true}},
  "semantic": {{"backend": "openai_compatible", "model": "stub-embedding",
               "base_url": "{base}", "timeout_ms": 60000}},
  "subc": {{"connection_file": "{conn}"}}}}"#,
            base = stub.base_url,
            conn = home.join("runtime/subc-connection.json").display(),
        ),
    )
    .unwrap();

    let meter_root = home.join("meter");
    std::fs::create_dir_all(&meter_root).unwrap();
    std::fs::write(meter_root.join("main.rs"), "fn main() {}\n").unwrap();
    let roots: Vec<PathBuf> = (0..root_count)
        .map(|index| {
            let root = home.join(format!("worktree_{index}"));
            write_corpus(&root, files_per_root, 10, 500 + index as u64);
            root
        })
        .collect();

    // Build and persist every root's semantic index. Each root's route is held
    // open by a long-running bash call so the build is never cancelled as
    // unbound; all roots build concurrently.
    {
        let daemon = HermeticDaemon::start(&subc_bin, &home);
        let deadline = Instant::now() + Duration::from_secs(30 * 60);
        while semantic_artifacts(&home) < root_count {
            assert!(
                Instant::now() < deadline,
                "semantic artifacts were not built in time"
            );
            thread::scope(|scope| {
                for (index, root) in roots.iter().enumerate() {
                    let (probe_bin, daemon) = (&probe_bin, &daemon);
                    scope.spawn(move || {
                        probe(
                            probe_bin,
                            daemon,
                            root,
                            &format!("build-{index}"),
                            "bash",
                            r#"{"command":"sleep 60"}"#,
                        )
                    });
                }
            });
        }
    }

    // Fresh daemon: every semantic index now comes from disk.
    let daemon = HermeticDaemon::start(&subc_bin, &home);
    let loads_before = logged_semantic_loads(&home).len();
    let baseline = census(&probe_bin, &daemon, &meter_root);
    row("baseline", baseline);
    // The load must finish while the route is still open so that it gets
    // installed: it takes about 10 s here, so each session runs one 15 s
    // command. A session shorter than the load is harmless (the finished load
    // is discarded because the root is unbound), which is why the command, not
    // a quick search, is used.
    for round in 0..env_usize("AFT_REPRO_ROUNDS", 1) {
        for (index, root) in roots.iter().enumerate() {
            // One short session: bind, run one command, close the route.
            probe(
                &probe_bin,
                &daemon,
                root,
                &format!("session-{round}-{index}"),
                "bash",
                &format!(
                    r#"{{"command":"sleep {}"}}"#,
                    env_usize("AFT_REPRO_SESSION_SECS", 15)
                ),
            );
        }
    }
    // Let the post-configure artifact loads finish on their worker threads.
    thread::sleep(Duration::from_secs(60));
    let after = census(&probe_bin, &daemon, &meter_root);
    row("after one short session per root", after);

    let loads: Vec<u64> = logged_semantic_loads(&home)
        .into_iter()
        .skip(loads_before)
        .collect();
    let loaded_vector_bytes: u64 = loads
        .iter()
        .map(|entries| entries * DIMENSION as u64 * 4)
        .sum();
    let unattributed_growth = after.unattributed.saturating_sub(baseline.unattributed);
    let attributed_growth = after.attributed.saturating_sub(baseline.attributed);
    eprintln!(
        "semantic loads from disk after the restart: {loads:?} ({:.1} MB of vectors alone)",
        loaded_vector_bytes as f64 / MB
    );
    eprintln!(
        "attributed grew {:.1} MB; unattributed grew {:.1} MB",
        attributed_growth as f64 / MB,
        unattributed_growth as f64 / MB
    );
    assert!(
        !loads.is_empty(),
        "no semantic index was loaded from disk; the reproduction did not run"
    );
    // Exact live bytes still held by semantic indexes loaded from disk, read
    // from the module's malloc stack log (MallocStackLogging is set in the
    // module's environment above). Nothing else in the process allocates
    // under `SemanticIndex::read_from_disk`.
    let module_pid = module_pid(&home).expect("find the hermetic aft module process");
    let semantic_live = live_bytes_under(module_pid, "SemanticIndex14read_from_disk");
    eprintln!(
        "live bytes allocated under SemanticIndex::read_from_disk: {:.1} MB",
        semantic_live as f64 / MB
    );
    // Every byte of a resident index should be attributed to a root. Allow
    // 32 MB for estimation error and the meter root's own state.
    assert!(
        semantic_live <= attributed_growth + 32 * 1024 * 1024,
        "{:.1} MB of semantic indexes loaded from disk are still live but the \
         census attributed only {:.1} MB more than at baseline (unattributed \
         grew {:.1} MB)",
        semantic_live as f64 / MB,
        attributed_growth as f64 / MB,
        unattributed_growth as f64 / MB,
    );
}

// ---------------------------------------------------------------------------
// In-process controls: workloads that do NOT retain memory.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Sample {
    live: u64,
    footprint: u64,
    malloc_small_dirty: u64,
}

fn sample() -> Sample {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let rc = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            usage.as_mut_ptr().cast(),
        )
    };
    assert_eq!(rc, 0, "proc_pid_rusage failed");
    Sample {
        live: LIVE_BYTES.load(Ordering::Relaxed) as u64,
        footprint: unsafe { usage.assume_init() }.ri_phys_footprint,
        malloc_small_dirty: malloc_small_dirty_bytes(),
    }
}

/// Dirty bytes in this process's "Malloc Small" category as footprint(1)
/// reports it: the category that grew to 8 GB in the production daemon. On
/// macOS 27 every allocation from 16 bytes to tens of kilobytes lands there.
fn malloc_small_dirty_bytes() -> u64 {
    let output = Command::new("/usr/bin/footprint")
        .arg(std::process::id().to_string())
        .output()
        .expect("run footprint");
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if !line.trim_end().ends_with("Malloc Small") {
            continue;
        }
        // The first column is the dirty size, e.g. "5088 KB" or "1.2 GB".
        let mut fields = line.split_whitespace();
        let (Some(number), Some(unit)) = (fields.next(), fields.next()) else {
            continue;
        };
        let value: f64 = number.parse().unwrap_or(0.0);
        let scale = match unit {
            "KB" => 1024.0,
            "MB" => 1024.0 * 1024.0,
            "GB" => 1024.0 * 1024.0 * 1024.0,
            _ => 1.0,
        };
        return (value * scale) as u64;
    }
    0
}

fn sample_row(label: &str, s: Sample) {
    eprintln!(
        "{label:<28} live={:>8.1}MB footprint={:>8.1}MB malloc_small_dirty={:>8.1}MB",
        s.live as f64 / MB,
        s.footprint as f64 / MB,
        s.malloc_small_dirty as f64 / MB,
    );
}

/// Print this process's footprint(1) categories, so a failure shows whether
/// the excess is allocator pages ("Malloc ...") or something else.
fn print_footprint_categories() {
    if let Ok(output) = Command::new("/usr/bin/footprint")
        .arg(std::process::id().to_string())
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if line.contains("Malloc") || line.contains("Reclaim") || line.contains("Footprint:") {
                eprintln!("    {line}");
            }
        }
    }
}

/// Once the work is over, dirty "Malloc Small" may exceed its starting point
/// only by the bytes still live plus 64 MB of allocator granularity. Total
/// footprint is printed but not asserted: freed large buffers (the 128 MB
/// search spill block among them) can stay dirty in "Malloc Large" for a while,
/// between 0.1 and 0.4 GB in these runs depending on system memory pressure,
/// which is a separate and bounded effect.
fn assert_nothing_retained(scenario: &str, baseline: Sample, end: Sample) {
    let live_growth = end.live.saturating_sub(baseline.live);
    let small_growth = end
        .malloc_small_dirty
        .saturating_sub(baseline.malloc_small_dirty);
    assert!(
        small_growth <= live_growth + 64 * 1024 * 1024,
        "{scenario}: dirty Malloc Small grew {:.1} MB while live bytes grew {:.1} MB",
        small_growth as f64 / MB,
        live_growth as f64 / MB,
    );
}

/// Repeated loads of one semantic artifact, each replacing the previous copy:
/// the shape of `loaded semantic index from disk` on every rebind.
#[test]
#[ignore = "slow macOS allocator measurement; negative control for the daemon growth"]
fn control_semantic_reload() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("corpus");
    let files = write_corpus(&root, env_usize("AFT_REPRO_FILES", 3000), 20, 11);
    let storage = temp.path().join("storage");
    {
        let mut embed = |texts: Vec<String>| -> Result<Vec<Vec<f32>>, String> {
            Ok(texts.iter().map(|_| vec![0.125f32; DIMENSION]).collect())
        };
        let index = SemanticIndex::build(&root, &files, &mut embed, 64).expect("semantic build");
        assert!(
            index.write_to_disk(&storage, "repro"),
            "write semantic artifact"
        );
    }
    let baseline = sample();
    sample_row("baseline", baseline);
    let mut current: Option<SemanticIndex> = None;
    for iteration in 0..env_usize("AFT_REPRO_ITERS", 8) {
        current = Some(
            SemanticIndex::read_from_disk(&storage, "repro", &root, false, None)
                .expect("semantic artifact loads"),
        );
        sample_row(&format!("reload {iteration}"), sample());
    }
    drop(current);
    let end = sample();
    sample_row("after dropping last index", end);
    print_footprint_categories();
    assert_nothing_retained("control_semantic_reload", baseline, end);
}

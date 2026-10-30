//! An already released build (v0.60.0) leaves v3 data alone.
//!
//! The v3 namespace (`<storage>/blobs/v3/<family>/`, including the member view
//! directories at `views/<scope>/` below it) is invisible to v0.60.0. These
//! tests run the real v0.60.0 binary over a storage root that holds a v3 tree
//! next to collectable v1 and v2 items, wait for its retention pass and its
//! view disk-limit enforcement to run after the seed, then run its
//! `aft cache prune-legacy`. The v3 tree's bytes and modes must not change,
//! and no v3 member may show up in a v2 registry.
//!
//! The release build cannot start retention sooner than five minutes after
//! its first configure, and repeats enforcement only every ten minutes, so
//! these tests are `#[ignore]`d and need the release binary:
//!
//! ```text
//! git clone --depth 1 --branch v0.60.0 https://github.com/cortexkit/aft.git v060
//! (cd v060 && cargo build --locked -p agent-file-tools --bin aft)
//! AFT_V0_60_0_BINARY=$PWD/v060/target/debug/aft \
//!   cargo test -p agent-file-tools --test integration -- --ignored v3_isolation::
//! ```
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Release constants of v0.60.0 that bound the waits below
/// (`storage_retention.rs:36-40`, `views/eviction.rs:65-68` at the tag).
const STARTUP_GRACE: Duration = Duration::from_secs(5 * 60);
const PASS_BUDGET: Duration = Duration::from_secs(5);
const ENFORCEMENT_INTERVAL: Duration = Duration::from_secs(10 * 60);
const ENFORCEMENT_DEADLINE: Duration = Duration::from_secs(60);
/// The slack each wait gets on top of the release's own schedule, and the
/// longest any single readiness or status poll may take.
const SLACK: Duration = Duration::from_secs(120);
/// v0.60.0 collects unreferenced v1 rows only once they are older than 15
/// minutes (`gc/mod.rs:21` at the tag); seeded rows are an hour old.
const V1_ROW_AGE_MS: u64 = 60 * 60 * 1000;
const V3_SCOPES: [&str; 2] = ["g7-view-member", "g7-base-member"];
const SEEDED_KEY: [u8; 32] = [0xA7; 32];

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The two cases run one at a time, so neither one's close-out runs while
/// the other's release process is alive.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn release_binary() -> PathBuf {
    let path = std::env::var_os("AFT_V0_60_0_BINARY")
        .map(PathBuf::from)
        .expect("set AFT_V0_60_0_BINARY to an aft binary built from tag v0.60.0");
    let version = Command::new(&path)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", path.display()));
    let version = String::from_utf8_lossy(&version.stdout);
    assert!(
        version.contains("0.60.0"),
        "not a v0.60.0 binary: {version}"
    );
    path
}

/// An OpenAI-compatible embedding endpoint on loopback that answers every
/// input with the same vector, so the semantic runtime loads offline.
struct Embedder {
    base_url: String,
}

impl Embedder {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let mut buf = [0; 8192];
                    let (start, length) = loop {
                        let Ok(n) = stream.read(&mut buf) else { return };
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let length: usize = String::from_utf8_lossy(&bytes[..end])
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                break (end + 4, length);
                            }
                        }
                    };
                    let body: Value =
                        serde_json::from_slice(&bytes[start..start + length]).unwrap_or_default();
                    let inputs = body["input"].as_array().map_or(1, Vec::len);
                    let data = (0..inputs)
                        .map(|index| json!({"index": index, "embedding": [0.25, 0.5, 0.75, 1.0]}))
                        .collect::<Vec<_>>();
                    let body = json!({ "data": data }).to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        Self { base_url }
    }
}

/// The release binary speaking its JSON-lines protocol.
struct Release {
    child: Child,
    stdin: ChildStdin,
    responses: mpsc::Receiver<Value>,
    next_id: u64,
}

fn hermetic(command: &mut Command, home: &Path) {
    for directory in ["home", "config", "data", "cache", "aft-cache"] {
        std::fs::create_dir_all(home.join(directory)).unwrap();
    }
    command
        .env_remove("AFT_STORAGE_DIR")
        .env("AFT_CACHE_DIR", home.join("aft-cache"))
        .env("HOME", home.join("home"))
        .env("USERPROFILE", home.join("home"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("FASTEMBED_CACHE_DIR", home.join("cache/fastembed"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join("home/.gitconfig"))
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "");
}

impl Release {
    fn spawn(binary: &Path, home: &Path, label: &str) -> Self {
        let mut command = Command::new(binary);
        hermetic(&mut command, home);
        let stderr = std::fs::File::create(home.join(format!("{label}.stderr"))).unwrap();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if sender.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            child,
            stdin,
            responses,
            next_id: 0,
        }
    }

    fn request(&mut self, mut request: Value) -> Value {
        self.next_id += 1;
        let id = format!("g7-{}", self.next_id);
        request["id"] = json!(id);
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        let deadline = Instant::now() + SLACK;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let response = self
                .responses
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response to {request} within {SLACK:?}"));
            if response["id"] == json!(id) {
                return response;
            }
        }
    }

    /// `second` flips `format_on_edit`, a setting that selects no artifact.
    /// The release answers a configure whose effective configuration equals
    /// the bound one without queuing configure maintenance, and only that
    /// maintenance schedules retention (`commands/configure.rs:3406-3505` and
    /// `7124-7144` at the tag).
    fn configure(
        &mut self,
        project: &Path,
        storage: &Path,
        embedder: &Embedder,
        second: bool,
    ) -> Value {
        let doc = json!({
            "format_on_edit": second,
            "views": { "enabled": true },
            "indexes": { "trigram": true, "semantic": true, "callgraph": true },
            "semantic": {
                "backend": "openai_compatible",
                "model": "g7-fixed-vector",
                "base_url": embedder.base_url,
                "timeout_ms": 5_000,
                "max_batch_size": 64,
                "max_files": 2_000
            }
        });
        let response = self.request(json!({
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "config": [{ "tier": "user", "source": "/g7/aft.jsonc", "doc": doc.to_string() }],
        }));
        assert_eq!(response["success"], true, "{response:#}");
        response
    }

    fn status(&mut self) -> Value {
        let response = self.request(json!({ "command": "status" }));
        assert_eq!(response["success"], true, "{response:#}");
        response
    }

    /// Waits `duration`, sending `status` every few seconds so the release's
    /// request drain, which starts view disk-limit enforcement, keeps running.
    fn idle(&mut self, duration: Duration) {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            self.status();
            thread::sleep(
                Duration::from_secs(5).min(until.saturating_duration_since(Instant::now())),
            );
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn stop(mut self) {
        drop(self.stdin);
        let deadline = Instant::now() + SLACK;
        while self.child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.wait();
    }
}

fn git(project: &Path, home: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    hermetic(&mut command, home);
    let output = command
        .current_dir(project)
        .args(["-c", "user.name=G7", "-c", "user.email=g7@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    storage: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let home = base.join("host");
        let project = base.join("project");
        let storage = base.join("storage");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(&storage).unwrap();
        for index in 0..8 {
            std::fs::write(
                project.join(format!("src/file{index}.rs")),
                format!("pub fn item{index}() -> u32 {{ {index} }}\n"),
            )
            .unwrap();
        }
        git(&project, &home, &["init", "--quiet"]);
        git(&project, &home, &["add", "."]);
        git(&project, &home, &["commit", "--quiet", "-m", "fixture"]);
        Self {
            _temp: temp,
            home,
            project,
            storage,
        }
    }

    fn log(&self, pid: u32) -> PathBuf {
        self.storage.join("logs").join(format!("aft-{pid}.log"))
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the release log's timestamp format, so log lines
/// compare with a seed time as strings.
fn utc_timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Enforcement report lines the release logged for `family`, with their
/// timestamps (`<timestamp> [aft] INFO view disk limits family=...`,
/// `views/eviction.rs:1186-1229` and `logging.rs:888-906` at the tag).
fn enforcement_lines(log: &Path, family: &str) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let needle = format!("view disk limits family={family} ");
    text.lines()
        .filter(|line| line.contains(&needle))
        .map(|line| {
            (
                line.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                line.to_owned(),
            )
        })
        .collect()
}

/// The single v2 family the release created for the checkout.
fn v2_family(storage: &Path) -> Option<String> {
    let mut families = std::fs::read_dir(storage.join("blobs/v2"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(families.len() <= 1, "more than one v2 family: {families:?}");
    families.pop()
}

/// Waits until the release has loaded the checkout's view runtime: its v2
/// family exists and its first enforcement pass has been logged.
fn wait_for_runtime(release: &mut Release, fixture: &Fixture) -> String {
    let deadline = Instant::now() + SLACK;
    loop {
        if let Some(family) = v2_family(&fixture.storage) {
            if !enforcement_lines(&fixture.log(release.pid()), &family).is_empty() {
                return family;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the release's view runtime did not load within {SLACK:?}"
        );
        release.status();
        thread::sleep(Duration::from_secs(2));
    }
}

/// The plane stores in `directory`: SQLite files with a `blob_payloads`
/// table. A v1 family directory also holds other stores, such as the OID
/// alias store.
fn sqlite_stores(directory: &Path) -> Vec<PathBuf> {
    plane_candidates(directory)
        .into_iter()
        .filter(|path| {
            let connection = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            connection.busy_timeout(Duration::from_secs(30)).unwrap();
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'blob_payloads'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
                == 1
        })
        .collect()
}

fn plane_candidates(directory: &Path) -> Vec<PathBuf> {
    let mut stores = std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "sqlite")
                        && path
                            .file_name()
                            .is_some_and(|name| name != "members.sqlite")
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    stores.sort();
    stores
}

fn open_store(path: &Path) -> rusqlite::Connection {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
    connection.busy_timeout(Duration::from_secs(30)).unwrap();
    connection
}

/// Inserts one row that nothing references into each v1 and v2 plane store
/// of the family. v1 rows are backdated past the release's age floor.
fn seed_collectable(storage: &Path, family: &str) -> Vec<PathBuf> {
    let payload = b"g7 collectable payload";
    let digest = blake3::hash(payload);
    let v1 = sqlite_stores(&storage.join("blobs").join(family));
    let v2 = sqlite_stores(&storage.join("blobs/v2").join(family));
    assert!(!v1.is_empty(), "the release created no v1 store");
    assert!(!v2.is_empty(), "the release created no v2 plane store");
    for path in &v1 {
        open_store(path)
            .execute(
                "INSERT INTO blob_payloads (full_key, payload, payload_digest, payload_schema, created_at_ms) VALUES (?1, ?2, ?3, 1, ?4)",
                rusqlite::params![&SEEDED_KEY[..], &payload[..], &digest.as_bytes()[..], now_ms() - V1_ROW_AGE_MS],
            )
            .unwrap();
    }
    for path in &v2 {
        open_store(path)
            .execute(
                "INSERT INTO blob_payloads (full_key, payload, payload_digest, payload_schema, ref_epoch, created_at_ms) VALUES (?1, ?2, ?3, 1, 0, ?4)",
                rusqlite::params![&SEEDED_KEY[..], &payload[..], &digest.as_bytes()[..], now_ms() - V1_ROW_AGE_MS],
            )
            .unwrap();
    }
    let seeded = v1.into_iter().chain(v2).collect::<Vec<_>>();
    eprintln!("seeded collectable rows into {seeded:?}");
    seeded
}

fn seeded_row_present(path: &Path) -> bool {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    connection.busy_timeout(Duration::from_secs(30)).unwrap();
    connection
        .query_row(
            "SELECT COUNT(*) FROM blob_payloads WHERE full_key = ?1",
            [&SEEDED_KEY[..]],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
        > 0
}

/// Writes the v3 tree in a child process that is then killed while it holds
/// the registry open, so `members.sqlite` keeps its `-wal` and `-shm` the way
/// a running or crashed v3 writer leaves them.
/// Kills and reaps the v3 writer child when dropped, including when a
/// readiness check panics, so a failed seed never leaves the child parked.
struct WriterChild(Child);

impl Drop for WriterChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the v3 writer child. With `never_ready` it parks without writing
/// anything, which only the guard test uses.
fn spawn_v3_writer(fixture: &Fixture, family: &str, never_ready: bool) -> (WriterChild, PathBuf) {
    let ready = fixture.home.join("v3-writer.ready");
    let mut command = Command::new(std::env::current_exe().unwrap());
    hermetic(&mut command, &fixture.home);
    command
        .args(["--exact", "v3_isolation::g7_v3_writer_child", "--nocapture"])
        .env("AFT_G7_V3_STORAGE", &fixture.storage)
        .env("AFT_G7_V3_FAMILY", family)
        .env("AFT_G7_V3_READY", &ready)
        .stdout(Stdio::null());
    if never_ready {
        command.env("AFT_G7_V3_NEVER_READY", "1");
    }
    (WriterChild(command.spawn().unwrap()), ready)
}

fn wait_until_ready(child: &mut WriterChild, ready: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "the v3 writer exited before it was ready"
        );
        assert!(Instant::now() < deadline, "the v3 writer never got ready");
        thread::sleep(Duration::from_millis(20));
    }
}

fn seed_v3(fixture: &Fixture, family: &str) {
    let (mut child, ready) = spawn_v3_writer(fixture, family, false);
    wait_until_ready(&mut child, &ready, SLACK);
    // Dropping the guard kills the writer while it holds the registry open.
    drop(child);
    let v3 = fixture.storage.join("blobs/v3").join(family);
    for name in [
        "members.sqlite",
        "members.sqlite-wal",
        "members.sqlite-shm",
        "semantic.sqlite",
    ] {
        assert!(v3.join(name).is_file(), "seeded v3 tree lacks {name}");
    }
    for scope in V3_SCOPES {
        let view = v3.join("views").join(scope);
        for name in ["pins", "readers", "pointer.sqlite"] {
            assert!(view.join(name).exists(), "{} lacks {name}", view.display());
        }
    }
}

#[test]
fn g7_v3_writer_child() {
    use aft::blob_store::v2::{FamilyKey, FamilyPlane};
    use aft::blob_store::v3;
    let Some(storage) = std::env::var_os("AFT_G7_V3_STORAGE").map(PathBuf::from) else {
        return;
    };
    let family = std::env::var("AFT_G7_V3_FAMILY").unwrap();
    let ready = PathBuf::from(std::env::var_os("AFT_G7_V3_READY").unwrap());
    if std::env::var_os("AFT_G7_V3_NEVER_READY").is_some() {
        loop {
            thread::park();
        }
    }
    let _space = v3::set_free_space_for_test(&storage, u64::MAX, u64::MAX / 2);
    let missing_root = storage.join("g7-missing-checkout");
    v3::publish(&storage, &family, 1 << 20, None, |publication| {
        publication
            .registry
            .register_member(V3_SCOPES[0], "view", Some(&missing_root))?;
        publication
            .registry
            .register_member(V3_SCOPES[1], "base", None)?;
        publication.put(
            &FamilyKey::new(FamilyPlane::Semantic, SEEDED_KEY),
            b"g7 v3 payload",
            1,
        )
    })
    .unwrap();
    for scope in V3_SCOPES {
        let view = v3::view_dir(&storage, &family, scope).unwrap();
        std::fs::create_dir_all(view.join("pins")).unwrap();
        std::fs::create_dir_all(view.join("readers/g1")).unwrap();
        std::fs::write(
            view.join("pins/g1.json"),
            json!({
                "family": family, "view": scope, "generation": "g1",
                "owner": { "pid": 1, "start_time": 1 },
                "created_at": 1, "renewed_at": 1
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(view.join("pins/g1.keys"), b"").unwrap();
        std::fs::write(
            view.join("readers/g1/1.host.1.0.json"),
            json!({ "pid": 1, "hostname": "g7-host", "created_at_ms": 1 }).to_string(),
        )
        .unwrap();
        let pointer = rusqlite::Connection::open(view.join("pointer.sqlite")).unwrap();
        pointer
            .execute_batch(
                "CREATE TABLE pointer (singleton INTEGER PRIMARY KEY, generation TEXT NOT NULL);
                 INSERT INTO pointer VALUES (1, 'g1');",
            )
            .unwrap();
        drop(pointer);
    }
    // Hold the registry open with committed, uncheckpointed frames in its WAL.
    let lock = v3::ReservationLock::acquire(&storage).unwrap();
    let mut registry = v3::RegistryWriter::open_existing(&storage, &family, &lock).unwrap();
    registry
        .with_barrier(|tx| {
            tx.execute(
                "UPDATE members SET last_publish_ms = 1 WHERE scope = ?1",
                [V3_SCOPES[0]],
            )?;
            Ok(())
        })
        .unwrap();
    std::fs::write(&ready, b"ready").unwrap();
    loop {
        thread::park();
    }
}

#[test]
fn a_v3_seed_publishes_using_its_storage_scoped_override() {
    let fixture = Fixture::new();
    seed_v3(&fixture, "0123456789abcdef");
}

/// A seed whose writer never gets ready fails, and the failure still kills
/// and reaps the writer instead of leaving it parked.
#[test]
fn a_seed_that_times_out_kills_its_writer() {
    let fixture = Fixture::new();
    let (child, ready) = spawn_v3_writer(&fixture, "0123456789abcdef", true);
    let pid = child.0.id() as libc::pid_t;
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut child = child;
        wait_until_ready(&mut child, &ready, Duration::from_secs(1));
    }));
    assert!(failed.is_err(), "the never-ready writer reported ready");
    // The guard reaped the child during the unwind, so its pid is gone; a
    // live or unreaped (zombie) child would still answer signal 0.
    // SAFETY: kill with signal 0 only checks whether the pid exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "the writer child {pid} outlived the failed seed");
}

/// Every entry under `blobs/v3/`, which includes the v3 view-dir root: its
/// type, `st_mode`, and the digest of its bytes or link target.
fn v3_digest(storage: &Path) -> BTreeMap<String, String> {
    use std::os::unix::fs::MetadataExt;
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<String, String>) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let content = if metadata.file_type().is_symlink() {
            format!("link {}", std::fs::read_link(path).unwrap().display())
        } else if metadata.is_dir() {
            let mut entries = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            entries.sort();
            for entry in &entries {
                walk(root, entry, out);
            }
            "dir".to_owned()
        } else {
            format!(
                "file {} {}",
                metadata.len(),
                blake3::hash(&std::fs::read(path).unwrap()).to_hex()
            )
        };
        out.insert(relative, format!("mode {:o} {content}", metadata.mode()));
    }
    let root = storage.join("blobs/v3");
    let mut out = BTreeMap::new();
    walk(&root, &root, &mut out);
    out
}

/// Runs the release's `aft cache prune-legacy`, dry and with `--yes`, and
/// returns what each printed.
fn prune_legacy(binary: &Path, fixture: &Fixture) -> Vec<String> {
    let mut printed = Vec::new();
    for yes in [false, true] {
        let mut command = Command::new(binary);
        hermetic(&mut command, &fixture.home);
        command
            .env("AFT_STORAGE_DIR", &fixture.storage)
            .args(["cache", "prune-legacy"]);
        if yes {
            command.arg("--yes");
        }
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        eprintln!(
            "prune-legacy{}: {:?}\n{stdout}{}",
            if yes { " --yes" } else { "" },
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        // The census is machine-wide, so an AFT process this test does not
        // own (another job on a shared runner) can make either mode refuse
        // (exit 3). The inventory walk and its listing come before that
        // census either way, and this test runs no release process here.
        let code = output.status.code();
        assert!(
            code == Some(0) || code == Some(3),
            "prune-legacy failed: {output:?}"
        );
        assert!(
            stdout.contains("Legacy sets under"),
            "prune-legacy listed nothing: {stdout}"
        );
        printed.push(stdout);
    }
    printed
}

fn assert_no_v3_member_in_v2(storage: &Path) {
    let mut registries = 0;
    for entry in std::fs::read_dir(storage.join("blobs/v2")).unwrap() {
        let path = entry.unwrap().path().join("members.sqlite");
        if !path.exists() {
            continue;
        }
        registries += 1;
        let connection = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let members = connection
            .prepare("SELECT scope, view_dir FROM members")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(!members.is_empty(), "{} lists no member", path.display());
        for (scope, view_dir) in members {
            assert!(
                !V3_SCOPES.contains(&scope.as_str()),
                "v3 member {scope} in {}",
                path.display()
            );
            assert!(
                !view_dir.contains("blobs/v3"),
                "v3 view dir {view_dir} in {}",
                path.display()
            );
        }
    }
    assert_eq!(registries, 1);
}

fn storage_retention(status: &Value) -> Option<&Value> {
    [
        &status["storage_retention"],
        &status["data"]["storage_retention"],
    ]
    .into_iter()
    .find(|value| value.is_object())
}

/// After the second configure: polls `status` until the release reports a
/// retention pass that started after it, and checks that pass.
fn retention_witness(release: &mut Release, log: &Path, second_configure_ms: u64) -> Value {
    let deadline = Instant::now() + SLACK;
    loop {
        let status = release.status();
        if let Some(report) = storage_retention(&status) {
            if report["at_ms"]
                .as_u64()
                .is_some_and(|at| at >= second_configure_ms)
            {
                assert_eq!(report["cancelled"], false, "{report:#}");
                assert_eq!(report["errors"], json!([]), "{report:#}");
                assert!(
                    report["examined_blobs"].as_u64().is_some_and(|n| n > 0),
                    "{report:#}"
                );
                return report.clone();
            }
        }
        if Instant::now() >= deadline {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            let lines = text
                .lines()
                .filter(|line| line.contains("retention") || line.contains("configure"))
                .collect::<Vec<_>>();
            panic!(
                "no retention pass after the second configure within {SLACK:?}: \
                 storage_retention={} storage_dir={}\nlog:\n{}",
                status["storage_retention"],
                status["storage_dir"],
                lines[lines.len().saturating_sub(60)..].join("\n")
            );
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// The close-out shared by both cases, once no release process runs.
fn close_out(
    binary: &Path,
    fixture: &Fixture,
    before: &BTreeMap<String, String>,
    seeded: &[PathBuf],
) {
    for path in seeded {
        assert!(
            !seeded_row_present(path),
            "seeded row survived in {}",
            path.display()
        );
    }
    let printed = prune_legacy(binary, fixture);
    assert_eq!(printed.len(), 2);
    let after = v3_digest(&fixture.storage);
    assert_eq!(&after, before, "the release changed the v3 tree");
    assert_no_v3_member_in_v2(&fixture.storage);
}

/// Freshly started: seed, start the release, configure, wait out its startup
/// grace, configure again and expect its first retention pass.
#[test]
#[ignore = "runs v0.60.0 for over seven minutes; set AFT_V0_60_0_BINARY"]
fn g7_freshly_started_release_leaves_v3_data_alone() {
    let _serial = serial();
    let binary = release_binary();
    let fixture = Fixture::new();
    let embedder = Embedder::start();
    // A first run creates the checkout's v1 and v2 stores to seed into.
    let mut bootstrap = Release::spawn(&binary, &fixture.home, "bootstrap");
    bootstrap.configure(&fixture.project, &fixture.storage, &embedder, false);
    let family = wait_for_runtime(&mut bootstrap, &fixture);
    bootstrap.stop();

    seed_v3(&fixture, &family);
    let seeded = seed_collectable(&fixture.storage, &family);
    let seeded_at = utc_timestamp(now_ms());
    let before = v3_digest(&fixture.storage);

    let mut release = Release::spawn(&binary, &fixture.home, "fresh");
    release.configure(&fixture.project, &fixture.storage, &embedder, false);
    release.idle(STARTUP_GRACE + PASS_BUDGET + SLACK);
    let second = now_ms();
    release.configure(&fixture.project, &fixture.storage, &embedder, true);
    let log = fixture.log(release.pid());
    let report = retention_witness(&mut release, &log, second);
    eprintln!("retention witness: {report}");
    let lines = enforcement_lines(&fixture.log(release.pid()), &family);
    let line = lines
        .iter()
        .find(|(at, _)| *at >= seeded_at)
        .unwrap_or_else(|| panic!("no enforcement pass logged after the seed: {lines:?}"));
    eprintln!("enforcement witness: {}", line.1);
    release.stop();
    close_out(&binary, &fixture, &before, &seeded);
}

/// Already running: configure, seed while the release runs, and configure
/// again once a post-seed enforcement pass is due and retention may start.
#[test]
#[ignore = "runs v0.60.0 for over thirteen minutes; set AFT_V0_60_0_BINARY"]
fn g7_already_running_release_leaves_v3_data_alone() {
    let _serial = serial();
    let binary = release_binary();
    let fixture = Fixture::new();
    let embedder = Embedder::start();
    let mut release = Release::spawn(&binary, &fixture.home, "running");
    let first = Instant::now();
    release.configure(&fixture.project, &fixture.storage, &embedder, false);
    let family = wait_for_runtime(&mut release, &fixture);

    seed_v3(&fixture, &family);
    let seeded = seed_collectable(&fixture.storage, &family);
    let seeded_at = utc_timestamp(now_ms());
    let before = v3_digest(&fixture.storage);

    release.idle(
        (ENFORCEMENT_INTERVAL + ENFORCEMENT_DEADLINE + SLACK).saturating_sub(first.elapsed()),
    );
    let second = now_ms();
    release.configure(&fixture.project, &fixture.storage, &embedder, true);
    let log = fixture.log(release.pid());
    let report = retention_witness(&mut release, &log, second);
    eprintln!("retention witness: {report}");
    let lines = enforcement_lines(&fixture.log(release.pid()), &family);
    let line = lines
        .iter()
        .find(|(at, _)| *at > seeded_at)
        .unwrap_or_else(|| panic!("no enforcement pass logged after the seed: {lines:?}"));
    eprintln!("enforcement witness: {}", line.1);
    release.stop();
    close_out(&binary, &fixture, &before, &seeded);
}

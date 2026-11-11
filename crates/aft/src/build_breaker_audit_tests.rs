use super::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::tempdir;

const COMMIT_CHILD_TEST: &str = "build_breaker::audit_matrix_tests::sqlite_commit_barrier_child";
const COMMIT_CHILD_DB: &str = "AFT_BREAKER_COMMIT_CHILD_DB";
const COMMIT_CHILD_KIND: &str = "AFT_BREAKER_COMMIT_CHILD_KIND";
const COMMIT_CHILD_BARRIER: &str = "AFT_BREAKER_COMMIT_CHILD_BARRIER";
const COMMIT_CHILD_SIGNAL: &str = "AFT_BREAKER_COMMIT_CHILD_SIGNAL";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DurableMarker {
    temp_identity: String,
    hostname: String,
    pid: u32,
    process_start_ms: u64,
    heartbeat_at_ms: u64,
    phase: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessEvidence {
    LiveExact,
    DeadExact,
    StartUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MarkerClassification {
    Live,
    Recent,
    Dead,
    Ambiguous,
}

#[derive(Default)]
struct FakeProcesses {
    states: HashMap<(u32, u64), ProcessEvidence>,
}

impl FakeProcesses {
    fn set(&mut self, marker: &DurableMarker, state: ProcessEvidence) {
        self.states
            .insert((marker.pid, marker.process_start_ms), state);
    }

    fn evidence(&self, marker: &DurableMarker) -> ProcessEvidence {
        self.states
            .get(&(marker.pid, marker.process_start_ms))
            .copied()
            .unwrap_or(ProcessEvidence::StartUnavailable)
    }
}

fn write_durable_marker(path: &Path, marker: &DurableMarker) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let temporary = path.with_extension("json.new");
    let mut file = File::create(&temporary).unwrap();
    serde_json::to_writer(&mut file, marker).unwrap();
    file.flush().unwrap();
    aft_store::sync::sync_all(&file, &temporary).unwrap();
    fs::rename(&temporary, path).unwrap();
}

fn classify_marker(
    path: &Path,
    local_hostname: &str,
    now_ms: u64,
    processes: &FakeProcesses,
) -> MarkerClassification {
    let Ok(bytes) = fs::read(path) else {
        return MarkerClassification::Ambiguous;
    };
    let Ok(marker) = serde_json::from_slice::<DurableMarker>(&bytes) else {
        return MarkerClassification::Ambiguous;
    };
    if marker.hostname != local_hostname || marker.heartbeat_at_ms > now_ms {
        return MarkerClassification::Ambiguous;
    }
    match processes.evidence(&marker) {
        ProcessEvidence::LiveExact => MarkerClassification::Live,
        ProcessEvidence::StartUnavailable => MarkerClassification::Ambiguous,
        ProcessEvidence::DeadExact => {
            if now_ms.saturating_sub(marker.heartbeat_at_ms) <= ATTEMPT_MARKER_RECENT_HEARTBEAT_MS {
                MarkerClassification::Recent
            } else {
                MarkerClassification::Dead
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum SweepEvidence {
    Dead,
    ClockRegression,
}

#[derive(Debug)]
struct SweepPass {
    checked: usize,
    deleted: usize,
    continuation: Option<String>,
}

struct DurableSweepFixture {
    artifacts: PathBuf,
    markers: PathBuf,
    state_db: PathBuf,
    hostname: String,
    processes: FakeProcesses,
    evidence: HashMap<String, SweepEvidence>,
}

impl DurableSweepFixture {
    fn new(base: &Path) -> Self {
        let artifacts = base.join("artifacts");
        let markers = base.join("markers");
        fs::create_dir_all(&artifacts).unwrap();
        fs::create_dir_all(&markers).unwrap();
        let state_db = base.join("sweep-state.sqlite");
        let conn = Connection::open(&state_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sweep_ambiguity (
                 temp_identity TEXT PRIMARY KEY,
                 ambiguous_since_ms INTEGER NOT NULL
             );
             CREATE TABLE sweep_cursor (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 last_identity TEXT
             );
             INSERT INTO sweep_cursor(singleton, last_identity) VALUES(1, NULL);",
        )
        .unwrap();
        Self {
            artifacts,
            markers,
            state_db,
            hostname: "fixture-host".to_string(),
            processes: FakeProcesses::default(),
            evidence: HashMap::new(),
        }
    }

    fn artifact_path(&self, identity: &str) -> PathBuf {
        self.artifacts.join(identity)
    }

    fn marker_path(&self, identity: &str) -> PathBuf {
        self.markers.join(format!("{identity}.json"))
    }

    fn create_artifact_set(&self, identity: &str, mtime_ms: u64) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let path = self.artifacts.join(format!("{identity}{suffix}"));
            fs::write(&path, suffix.as_bytes()).unwrap();
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_system_time(UNIX_EPOCH + Duration::from_millis(mtime_ms)),
            )
            .unwrap();
        }
    }

    fn artifact_set_exists(&self, identity: &str) -> bool {
        ["", "-journal", "-wal", "-shm"]
            .into_iter()
            .all(|suffix| self.artifacts.join(format!("{identity}{suffix}")).exists())
    }

    fn artifact_set_absent(&self, identity: &str) -> bool {
        ["", "-journal", "-wal", "-shm"]
            .into_iter()
            .all(|suffix| !self.artifacts.join(format!("{identity}{suffix}")).exists())
    }

    fn write_marker(&self, marker: &DurableMarker) {
        write_durable_marker(&self.marker_path(&marker.temp_identity), marker);
    }

    fn candidate_identities(&self) -> Vec<String> {
        let mut identities = BTreeSet::new();
        for entry in fs::read_dir(&self.artifacts).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.contains(".sqlite.tmp.") {
                continue;
            }
            let base = ["-journal", "-wal", "-shm"]
                .into_iter()
                .find_map(|suffix| name.strip_suffix(suffix))
                .unwrap_or(&name);
            identities.insert(base.to_string());
        }
        identities.into_iter().collect()
    }

    fn continuation_cursor(&self) -> Option<String> {
        Connection::open(&self.state_db)
            .unwrap()
            .query_row(
                "SELECT last_identity FROM sweep_cursor WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn ambiguity_since(&self, identity: &str) -> Option<u64> {
        Connection::open(&self.state_db)
            .unwrap()
            .query_row(
                "SELECT ambiguous_since_ms FROM sweep_ambiguity WHERE temp_identity = ?1",
                params![identity],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .unwrap()
            .map(|value| value.max(0) as u64)
    }

    fn sweep(&self, now_ms: u64, check_markers: bool) -> SweepPass {
        let conn = Connection::open(&self.state_db).unwrap();
        let cursor = conn
            .query_row(
                "SELECT last_identity FROM sweep_cursor WHERE singleton = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap();
        let candidates = self.candidate_identities();
        let selected = candidates
            .iter()
            .filter(|identity| cursor.as_ref().is_none_or(|cursor| *identity > cursor))
            .take(SWEEP_STAT_CHECK_CAP)
            .cloned()
            .collect::<Vec<_>>();
        let mut deleted = 0;

        for identity in &selected {
            let base = self.artifact_path(identity);
            let modified_ms = fs::metadata(&base)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64);
            let mut ambiguous = modified_ms.is_none_or(|mtime| mtime > now_ms);
            let fresh = modified_ms
                .is_some_and(|mtime| now_ms.saturating_sub(mtime) < TEMP_DELETE_AGE_FLOOR_MS);
            if fresh && !ambiguous {
                continue;
            }

            if check_markers {
                let marker_path = self.marker_path(identity);
                if marker_path.exists() {
                    match classify_marker(&marker_path, &self.hostname, now_ms, &self.processes) {
                        MarkerClassification::Live | MarkerClassification::Recent => continue,
                        MarkerClassification::Ambiguous => ambiguous = true,
                        MarkerClassification::Dead => {}
                    }
                }
            }
            if matches!(
                self.evidence.get(identity),
                Some(SweepEvidence::ClockRegression)
            ) {
                ambiguous = true;
            }

            if ambiguous {
                conn.execute(
                    "INSERT OR IGNORE INTO sweep_ambiguity(temp_identity, ambiguous_since_ms)
                     VALUES(?1, ?2)",
                    params![identity, now_ms.min(i64::MAX as u64) as i64],
                )
                .unwrap();
            }
            let ambiguity_since = conn
                .query_row(
                    "SELECT ambiguous_since_ms FROM sweep_ambiguity WHERE temp_identity = ?1",
                    params![identity],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .unwrap()
                .map(|since| since.max(0) as u64);
            if ambiguity_since
                .is_some_and(|since| now_ms.saturating_sub(since) <= SWEEP_AMBIGUITY_TTL_MS)
            {
                continue;
            }
            if ambiguity_since.is_none()
                && !matches!(self.evidence.get(identity), Some(SweepEvidence::Dead))
            {
                continue;
            }

            for suffix in ["", "-journal", "-wal", "-shm"] {
                match fs::remove_file(self.artifacts.join(format!("{identity}{suffix}"))) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => panic!("failed to remove sweep fixture artifact: {error}"),
                }
            }
            deleted += 1;
        }

        let continuation = selected.last().and_then(|last| {
            candidates
                .iter()
                .any(|candidate| candidate > last)
                .then(|| last.clone())
        });
        conn.execute(
            "UPDATE sweep_cursor SET last_identity = ?1 WHERE singleton = 1",
            params![continuation],
        )
        .unwrap();
        SweepPass {
            checked: selected.len(),
            deleted,
            continuation,
        }
    }
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "child did not reach SQLite commit barrier {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn initialize_batch_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         CREATE TABLE batches(kind TEXT NOT NULL, payload TEXT NOT NULL);
         CREATE TABLE staging_meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
         INSERT INTO staging_meta(key, value) VALUES('committed_extracted_bytes', 0);",
    )
    .unwrap();
}

fn kill_at_commit_barrier(kind: &str, barrier: &str) -> (PathBuf, tempfile::TempDir) {
    let temp = tempdir().unwrap();
    let db = temp.path().join("staging.sqlite");
    let signal = temp.path().join("commit-barrier.reached");
    initialize_batch_db(&db);

    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(COMMIT_CHILD_TEST)
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(COMMIT_CHILD_DB, &db)
        .env(COMMIT_CHILD_KIND, kind)
        .env(COMMIT_CHILD_BARRIER, barrier)
        .env(COMMIT_CHILD_SIGNAL, &signal)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_file(&signal);
    child.kill().unwrap();
    let _ = child.wait().unwrap();
    (db, temp)
}

fn durable_batch_state(path: &Path, kind: &str) -> (u64, u64) {
    let conn = Connection::open(path).unwrap();
    let rows = conn
        .query_row(
            "SELECT COUNT(*) FROM batches WHERE kind = ?1",
            params![kind],
            |row| row.get::<_, i64>(0),
        )
        .unwrap() as u64;
    let counter = conn
        .query_row(
            "SELECT value FROM staging_meta WHERE key = 'committed_extracted_bytes'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap() as u64;
    (rows, counter)
}

#[test]
fn sqlite_commit_barrier_child() {
    let Some(db) = std::env::var_os(COMMIT_CHILD_DB) else {
        return;
    };
    let kind = std::env::var(COMMIT_CHILD_KIND).unwrap();
    let barrier = std::env::var(COMMIT_CHILD_BARRIER).unwrap();
    let signal = PathBuf::from(std::env::var_os(COMMIT_CHILD_SIGNAL).unwrap());
    let mut conn = Connection::open(db).unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute(
        "INSERT INTO batches(kind, payload) VALUES(?1, 'durable batch')",
        params![kind],
    )
    .unwrap();
    tx.execute(
        "UPDATE staging_meta SET value = value + 17
         WHERE key = 'committed_extracted_bytes'",
        [],
    )
    .unwrap();
    if barrier == "before" {
        fs::write(&signal, b"before commit").unwrap();
        std::thread::sleep(Duration::from_secs(30));
        return;
    }
    tx.commit().unwrap();
    fs::write(&signal, b"after commit").unwrap();
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
fn kill_during_extract_and_reconciliation_commit_is_atomic_and_credit_uses_only_counter() {
    for kind in ["extract", "reconciliation"] {
        let (rolled_back, _rolled_back_temp) = kill_at_commit_barrier(kind, "before");
        assert_eq!(
            durable_batch_state(&rolled_back, kind),
            (0, 0),
            "a kill before {kind} commit must expose neither rows nor counter credit"
        );

        let (committed, _committed_temp) = kill_at_commit_barrier(kind, "after");
        let (rows, counter) = durable_batch_state(&committed, kind);
        assert_eq!(
            (rows, counter),
            (1, 17),
            "a kill after {kind} commit must expose its rows and counter together"
        );

        let breaker_dir = tempdir().unwrap();
        let breaker = BuildDeathBreaker::open(breaker_dir.path().join("breaker.sqlite")).unwrap();
        let key = BreakerKey::new(format!("root-{kind}"), BuildDomain::CallgraphCold, "corpus");
        let BreakerAdmission::Admitted(credited_attempt) = breaker.admit_at(&key, 0, 1).unwrap()
        else {
            panic!("fresh fixture unexpectedly suspended");
        };
        breaker
            .record_attributed_death_at(&key, &credited_attempt.attempt_id, counter, 0, 2)
            .unwrap();

        // A row committed outside an extraction transaction changes durable row
        // evidence without changing the only counter that can grant credit.
        Connection::open(&committed)
            .unwrap()
            .execute(
                "INSERT INTO batches(kind, payload) VALUES(?1, 'non-credit row')",
                params![kind],
            )
            .unwrap();
        let (_, unchanged_counter) = durable_batch_state(&committed, kind);
        let BreakerAdmission::Admitted(uncredited_attempt) =
            breaker.admit_at(&key, unchanged_counter, 3).unwrap()
        else {
            panic!("two deaths cannot suspend the fixture");
        };
        breaker
            .record_attributed_death_at(
                &key,
                &uncredited_attempt.attempt_id,
                unchanged_counter,
                0,
                4,
            )
            .unwrap();
        let tallies = Connection::open(breaker.path())
            .unwrap()
            .query_row(
                "SELECT zero_credit_deaths, credited_deaths FROM breaker_records
                 WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
                params![key.root_id, key.domain.as_str(), key.corpus_fingerprint],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap();
        assert_eq!(tallies, (1, 1));
    }
}

#[test]
fn marker_recency_boundary_requires_exact_dead_process_after_fifteen_seconds() {
    let temp = tempdir().unwrap();
    let marker_path = temp.path().join("attempt.json");
    let marker = DurableMarker {
        temp_identity: "root.staging.sqlite.tmp.resume".to_string(),
        hostname: "fixture-host".to_string(),
        pid: 42,
        process_start_ms: 900,
        heartbeat_at_ms: 1_000_000,
        phase: "extracting".to_string(),
    };
    write_durable_marker(&marker_path, &marker);
    let mut processes = FakeProcesses::default();
    processes.set(&marker, ProcessEvidence::DeadExact);

    assert_eq!(
        classify_marker(
            &marker_path,
            "fixture-host",
            marker.heartbeat_at_ms + ATTEMPT_MARKER_RECENT_HEARTBEAT_MS,
            &processes,
        ),
        MarkerClassification::Recent
    );
    assert_eq!(
        classify_marker(
            &marker_path,
            "fixture-host",
            marker.heartbeat_at_ms + ATTEMPT_MARKER_RECENT_HEARTBEAT_MS + 1,
            &processes,
        ),
        MarkerClassification::Dead
    );

    processes.set(&marker, ProcessEvidence::LiveExact);
    assert_eq!(
        classify_marker(
            &marker_path,
            "fixture-host",
            marker.heartbeat_at_ms + 10 * ATTEMPT_MARKER_RECENT_HEARTBEAT_MS,
            &processes,
        ),
        MarkerClassification::Live,
        "an exact live process protects an old same-host heartbeat"
    );
    processes.set(&marker, ProcessEvidence::StartUnavailable);
    assert_eq!(
        classify_marker(
            &marker_path,
            "fixture-host",
            marker.heartbeat_at_ms + ATTEMPT_MARKER_RECENT_HEARTBEAT_MS + 1,
            &processes,
        ),
        MarkerClassification::Ambiguous,
        "a stale heartbeat is not dead evidence without exact process-start data"
    );
}

#[test]
fn adopted_temp_marker_protection_is_load_bearing_and_covers_sqlite_sidecars() {
    let temp = tempdir().unwrap();
    let now = 10 * TEMP_DELETE_AGE_FLOOR_MS;
    let identity = "root.staging.sqlite.tmp.resume";
    let mut sweep = DurableSweepFixture::new(temp.path());
    sweep
        .evidence
        .insert(identity.to_string(), SweepEvidence::Dead);
    let marker = DurableMarker {
        temp_identity: identity.to_string(),
        hostname: sweep.hostname.clone(),
        pid: 77,
        process_start_ms: 1234,
        heartbeat_at_ms: now - ATTEMPT_MARKER_RECENT_HEARTBEAT_MS - 1,
        phase: "extracting".to_string(),
    };
    sweep.write_marker(&marker);
    sweep.processes.set(&marker, ProcessEvidence::LiveExact);

    // Negative control: bypassing the reference check removes the resumed
    // writer's base, journal, WAL, and shared-memory files.
    sweep.create_artifact_set(identity, now - TEMP_DELETE_AGE_FLOOR_MS - 1);
    let unprotected = sweep.sweep(now, false);
    assert_eq!(unprotected.deleted, 1);
    assert!(sweep.artifact_set_absent(identity));

    sweep.create_artifact_set(identity, now - TEMP_DELETE_AGE_FLOOR_MS - 1);
    let protected = sweep.sweep(now, true);
    assert_eq!(protected.deleted, 0);
    assert!(sweep.artifact_set_exists(identity));
}

#[test]
fn ambiguous_sweep_evidence_retains_once_through_seven_day_boundary() {
    let temp = tempdir().unwrap();
    let start = 20 * TEMP_DELETE_AGE_FLOOR_MS;
    let mut sweep = DurableSweepFixture::new(temp.path());
    let identities = [
        "cross-host.sqlite.tmp.resume",
        "malformed.sqlite.tmp.resume",
        "future-mtime.sqlite.tmp.resume",
        "clock-regression.sqlite.tmp.resume",
    ];
    for identity in identities {
        sweep.create_artifact_set(identity, start - TEMP_DELETE_AGE_FLOOR_MS - 1);
        sweep
            .evidence
            .insert(identity.to_string(), SweepEvidence::Dead);
    }
    sweep.evidence.insert(
        "clock-regression.sqlite.tmp.resume".to_string(),
        SweepEvidence::ClockRegression,
    );
    sweep.create_artifact_set("future-mtime.sqlite.tmp.resume", start + 1);
    let cross_host = DurableMarker {
        temp_identity: "cross-host.sqlite.tmp.resume".to_string(),
        hostname: "another-host".to_string(),
        pid: 90,
        process_start_ms: 500,
        heartbeat_at_ms: start - 1,
        phase: "extracting".to_string(),
    };
    sweep.write_marker(&cross_host);
    fs::write(
        sweep.marker_path("malformed.sqlite.tmp.resume"),
        b"not marker metadata",
    )
    .unwrap();

    assert_eq!(sweep.sweep(start, true).deleted, 0);
    for identity in identities {
        assert_eq!(sweep.ambiguity_since(identity), Some(start));
    }

    let before_boundary = start + SWEEP_AMBIGUITY_TTL_MS - 1_000;
    assert_eq!(sweep.sweep(before_boundary, true).deleted, 0);
    assert_eq!(sweep.sweep(start + SWEEP_AMBIGUITY_TTL_MS, true).deleted, 0);
    for identity in identities {
        assert!(sweep.artifact_set_exists(identity));
        assert_eq!(
            sweep.ambiguity_since(identity),
            Some(start),
            "repeated scans must not refresh ambiguous_since"
        );
    }

    let expired = sweep.sweep(start + SWEEP_AMBIGUITY_TTL_MS + 1, true);
    assert_eq!(expired.deleted, identities.len());
    for identity in identities {
        assert!(sweep.artifact_set_absent(identity));
    }
}

#[test]
fn ordinary_temp_floor_and_sixty_four_check_continuation_bound_each_pass() {
    let fresh_temp = tempdir().unwrap();
    let now = 30 * TEMP_DELETE_AGE_FLOOR_MS;
    let mut fresh_sweep = DurableSweepFixture::new(fresh_temp.path());
    let fresh = "fresh.sqlite.tmp.resume";
    fresh_sweep.create_artifact_set(fresh, now - TEMP_DELETE_AGE_FLOOR_MS + 1);
    fresh_sweep
        .evidence
        .insert(fresh.to_string(), SweepEvidence::Dead);
    let fresh_pass = fresh_sweep.sweep(now, true);
    assert_eq!(fresh_pass.checked, 1);
    assert_eq!(fresh_pass.deleted, 0);
    assert!(fresh_sweep.artifact_set_exists(fresh));

    let capped_temp = tempdir().unwrap();
    let mut capped_sweep = DurableSweepFixture::new(capped_temp.path());
    for index in 0..70 {
        let identity = format!("candidate-{index:03}.sqlite.tmp.resume");
        capped_sweep.create_artifact_set(&identity, now - TEMP_DELETE_AGE_FLOOR_MS - 1);
        capped_sweep.evidence.insert(identity, SweepEvidence::Dead);
    }
    let first = capped_sweep.sweep(now, true);
    assert_eq!(first.checked, SWEEP_STAT_CHECK_CAP);
    assert_eq!(first.deleted, SWEEP_STAT_CHECK_CAP);
    assert!(first.continuation.is_some());
    assert_eq!(
        capped_sweep.continuation_cursor(),
        first.continuation,
        "the next maintenance pass must recover its cursor from durable state"
    );
    assert_eq!(capped_sweep.candidate_identities().len(), 6);

    let second = capped_sweep.sweep(now + 1, true);
    assert_eq!(second.checked, 6);
    assert_eq!(second.deleted, 6);
    assert!(second.continuation.is_none());
    assert!(capped_sweep.continuation_cursor().is_none());
    assert!(capped_sweep.candidate_identities().is_empty());
}

#[test]
fn marker_clock_anomalies_are_ambiguous() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("attempt.json");
    let marker = DurableMarker {
        temp_identity: "future.sqlite.tmp.resume".to_string(),
        hostname: "fixture-host".to_string(),
        pid: 91,
        process_start_ms: 700,
        heartbeat_at_ms: 50_001,
        phase: "reconciling".to_string(),
    };
    write_durable_marker(&path, &marker);
    let mut processes = FakeProcesses::default();
    processes.set(&marker, ProcessEvidence::DeadExact);
    assert_eq!(
        classify_marker(&path, "fixture-host", 50_000, &processes),
        MarkerClassification::Ambiguous
    );
}

#[test]
fn heartbeat_interval_constant_preserves_three_sample_recency_window() {
    assert_eq!(
        ATTEMPT_MARKER_RECENT_HEARTBEAT_MS,
        3 * ATTEMPT_MARKER_HEARTBEAT_INTERVAL_MS
    );
    assert!(SystemTime::now().duration_since(UNIX_EPOCH).is_ok());
}

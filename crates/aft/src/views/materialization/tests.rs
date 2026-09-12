use super::*;
use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};
use rusqlite::types::Value;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    blobs: std::path::PathBuf,
    base: Manifest,
    next: Manifest,
}

fn manifest(blobs: &Connection, files: &[(&str, &str)]) -> Manifest {
    Manifest::new(files.iter().map(|(path, source)| {
        let blob = join::CallgraphBlob::extract(source, "typescript", "fixture").unwrap();
        let payload = blob.to_bytes().unwrap();
        let key = blake3::hash(&payload);
        blobs
            .execute(
                "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2)",
                params![key.as_bytes().as_slice(), payload],
            )
            .unwrap();
        (
            RelPath::new(path.as_bytes()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    callgraph: Some(key.to_hex().to_string()),
                    semantic: None,
                },
                resolution_input: false,
            },
        )
    }))
    .unwrap()
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let blobs = dir.path().join("blobs.sqlite");
    let conn = Connection::open(&blobs).unwrap();
    conn.execute_batch(
        "CREATE TABLE blob_payloads(full_key BLOB PRIMARY KEY, payload BLOB NOT NULL)",
    )
    .unwrap();
    let caller = "import { target } from './target'; export function caller() { return target(); }";
    let base = manifest(
        &conn,
        &[
            ("caller.ts", caller),
            ("target.ts", "export function target() { return 1; }"),
            ("removed.ts", "export function removed() { return 0; }"),
            ("untouched.ts", "export function untouched() { return 8; }"),
        ],
    );
    let next = manifest(
        &conn,
        &[
            ("caller.ts", caller),
            (
                "target.ts",
                "export function before() {} export function target() { return 2; }",
            ),
            ("added.ts", "export function added() { return 4; }"),
            ("untouched.ts", "export function untouched() { return 8; }"),
        ],
    );
    Fixture {
        dir,
        blobs,
        base,
        next,
    }
}

fn snapshot(path: &Path) -> BTreeMap<String, Vec<String>> {
    let conn = Connection::open(path).unwrap();
    let tables = conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap()
        .query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    tables
        .into_iter()
        .map(|table| {
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = stmt.column_count();
            let mut rows = stmt
                .query_map([], |row| {
                    (0..columns)
                        .map(|i| row.get::<_, Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map(|values| format!("{values:?}"))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            rows.sort();
            (table, rows)
        })
        .collect()
}

fn prepare(f: &Fixture) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = f.dir.path().join("base.sqlite");
    let copy = f.dir.path().join("copy.sqlite");
    materialize_manifest_view_database(&base, &f.blobs, &f.base).unwrap();
    std::fs::copy(&base, &copy).unwrap();
    (base, copy)
}

#[test]
fn incremental_rows_match_cold_with_cross_file_relink() {
    let f = fixture();
    let (base, copy) = prepare(&f);
    let before = snapshot(&base);
    let stats = apply_manifest_diff(&copy, &f.base, &f.next, &f.blobs).unwrap();
    let cold = f.dir.path().join("cold.sqlite");
    materialize_manifest_view_database(&cold, &f.blobs, &f.next).unwrap();
    let actual = snapshot(&copy);
    for (table, expected) in snapshot(&cold) {
        assert_eq!(actual[&table], expected, "table {table}");
    }
    assert!(
        stats.relinked_inserted > 0,
        "fixture must exercise incoming edges"
    );
    assert_eq!(snapshot(&base), before, "published base remains readable");
}

#[test]
fn incremental_writes_only_owned_rows_and_relinks() {
    let f = fixture();
    let (_, copy) = prepare(&f);
    let stats = apply_manifest_diff(&copy, &f.base, &f.next, &f.blobs).unwrap();
    println!("incremental counts: {stats:?}");
    assert_eq!(
        stats,
        MaterializeStats {
            deleted: 4,
            inserted: 5,
            relinked_deleted: 2,
            relinked_inserted: 2
        }
    );
    assert_eq!(stats.rows_written(), 13);
}

#[test]
fn mismatched_base_and_schema_are_rejected_without_changes() {
    let f = fixture();
    let (_, copy) = prepare(&f);
    let before = snapshot(&copy);
    assert!(apply_manifest_diff(&copy, &f.next, &f.base, &f.blobs).is_err());
    assert_eq!(snapshot(&copy), before);
    Connection::open(&copy)
        .unwrap()
        .execute(
            "UPDATE meta SET v='obsolete' WHERE k='view_materialization_version'",
            [],
        )
        .unwrap();
    let before = snapshot(&copy);
    assert!(apply_manifest_diff(&copy, &f.base, &f.next, &f.blobs).is_err());
    assert_eq!(snapshot(&copy), before);
}

#[test]
fn missing_blob_rolls_back_deletions_and_fingerprint() {
    let f = fixture();
    let (_, copy) = prepare(&f);
    let before = snapshot(&copy);
    Connection::open(&f.blobs)
        .unwrap()
        .execute("DELETE FROM blob_payloads", [])
        .unwrap();
    assert!(apply_manifest_diff(&copy, &f.base, &f.next, &f.blobs).is_err());
    assert_eq!(snapshot(&copy), before);
}

#[test]
fn identical_manifest_performs_zero_writes() {
    let f = fixture();
    let (_, copy) = prepare(&f);
    let before = std::fs::read(&copy).unwrap();
    assert_eq!(
        apply_manifest_diff(&copy, &f.base, &f.base, &f.blobs).unwrap(),
        MaterializeStats::default()
    );
    assert_eq!(std::fs::read(&copy).unwrap(), before);
}

#[cfg(target_os = "macos")]
fn usage() -> (u64, u64, f64) {
    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }
    let mut buffer = [0_u64; 64];
    let result = unsafe {
        proc_pid_rusage(
            std::process::id() as libc::c_int,
            4,
            buffer.as_mut_ptr().cast(),
        )
    };
    assert_eq!(result, 0, "Darwin write accounting is required");
    // RUSAGE_INFO_V4 has a 16-byte UUID followed by eight-byte counters.
    let mut cpu = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, cpu.as_mut_ptr()) },
        0
    );
    let cpu = unsafe { cpu.assume_init() };
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
    (
        buffer[2 + 17],
        buffer[2 + 27],
        seconds(cpu.ru_utime) + seconds(cpu.ru_stime),
    )
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires offline real manifests and blob database; never point at live storage"]
fn bench_real_manifest_diff() {
    let input = std::path::PathBuf::from(
        std::env::var_os("AFT_VIEW_DIFF_INPUT").expect("offline input directory"),
    );
    let load =
        |name: &str| Manifest::from_json_bytes(&std::fs::read(input.join(name)).unwrap()).unwrap();
    let base = load("base.json");
    let next = load("next.json");
    let blobs = input.join("callgraph.sqlite");
    let changed = base
        .entries()
        .chain(next.entries())
        .filter(|(p, _)| base.get(p) != next.get(p))
        .map(|(p, _)| p.clone())
        .collect::<BTreeSet<_>>();
    println!(
        "real manifest diff: base={} next={} changed={}",
        base.entries().count(),
        next.entries().count(),
        changed.len()
    );
    let temp = tempfile::tempdir_in(&input).unwrap();
    let original = temp.path().join("base.sqlite");
    materialize_manifest_view_database(&original, &blobs, &base).unwrap();
    let mut outputs = Vec::new();
    for incremental in [false, true] {
        let db = temp.path().join(if incremental {
            "incremental.sqlite"
        } else {
            "cold.sqlite"
        });
        std::fs::copy(&original, &db).unwrap();
        let keeper = Connection::open(&db).unwrap();
        keeper
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let before = usage();
        let start = std::time::Instant::now();
        let stats = materialize(&db, &blobs, &next, incremental.then_some(&base)).unwrap();
        let elapsed = start.elapsed().as_secs_f64();
        let after = usage();
        let wal = std::fs::metadata(format!("{}-wal", db.display()))
            .unwrap()
            .len();
        println!("incremental={incremental} wall_s={elapsed:.3} cpu_s={:.3} physical_bytes={} logical_bytes={} wal_bytes={wal} stats={stats:?}", after.2-before.2, after.0-before.0, after.1-before.1);
        outputs.push(snapshot(&db));
    }
    assert_eq!(outputs[0], outputs[1], "real manifest row parity");
}

#[test]
fn added_and_removed_targets_relink_previously_unresolved_callers() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let source = "import { target } from './target'; export function caller() { return target(); }";
    let absent = manifest(&conn, &[("caller.ts", source)]);
    let present = manifest(
        &conn,
        &[
            ("caller.ts", source),
            ("target.ts", "export function target() { return 1; }"),
        ],
    );
    for (index, (base, next)) in [(&absent, &present), (&present, &absent)]
        .into_iter()
        .enumerate()
    {
        let db = f.dir.path().join(format!("transition-{index}.sqlite"));
        let cold = f.dir.path().join(format!("expected-{index}.sqlite"));
        materialize_manifest_view_database(&db, &f.blobs, base).unwrap();
        apply_manifest_diff(&db, base, next, &f.blobs).unwrap();
        materialize_manifest_view_database(&cold, &f.blobs, next).unwrap();
        assert_eq!(snapshot(&db), snapshot(&cold));
    }
}

#[test]
fn selected_join_seam_matches_existing_cold_join_and_retains_missing_candidates() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let reader = ManifestViewBlobReader { connection: &conn };
    let old = join::JoinResult::from_manifest(&f.base, &reader).unwrap();
    let cold = join::join_selected_manifest(&f.base, &reader, None, &BTreeMap::new()).unwrap();
    assert_eq!(
        old.canonical_serialization(),
        cold.result.canonical_serialization()
    );
    assert!(
        cold.bindings["caller.ts"]
            .dependencies
            .contains("target.tsx"),
        "absent alternatives must be persisted"
    );
    let selected = BTreeSet::from([
        "target.ts".to_string(),
        "added.ts".to_string(),
        "caller.ts".to_string(),
    ]);
    let next =
        join::join_selected_manifest(&f.next, &reader, Some(&selected), &cold.bindings).unwrap();
    let all = join::JoinResult::from_manifest(&f.next, &reader).unwrap();
    assert_eq!(
        next.result.rows,
        all.rows
            .into_iter()
            .filter(|row| selected.contains(std::str::from_utf8(&row.caller_path).unwrap()))
            .collect()
    );
}

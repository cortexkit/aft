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
        let language = if path.ends_with(".rs") {
            "rust"
        } else {
            "typescript"
        };
        let blob = join::CallgraphBlob::extract(source, language, "fixture").unwrap();
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
            relinked_inserted: 2,
            dependency_deleted: 2,
            dependency_inserted: 2,
            dependent_files: 1,
            resolved_files: 3,
            resolved_refs: 2,
            full_resolution: false,
        }
    );
    assert_eq!(stats.graph_rows_written(), 13);
    assert_eq!(stats.rows_written(), 17);
    assert_eq!(stats.dependent_files, 1);
    assert_eq!(stats.resolved_files, 3);
    assert!(!stats.full_resolution);
}

#[test]
fn mismatched_base_is_rejected_and_old_schema_is_cold_upgraded() {
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
    let stats = apply_manifest_diff(&copy, &f.base, &f.next, &f.blobs).unwrap();
    assert!(stats.full_resolution);
    let cold = f.dir.path().join("upgraded-cold.sqlite");
    materialize_manifest_view_database(&cold, &f.blobs, &f.next).unwrap();
    assert_eq!(snapshot(&copy), snapshot(&cold));
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
    let temp = tempfile::tempdir_in(&input).unwrap().keep();
    println!("measurement databases: {}", temp.display());
    let original = temp.join("base.sqlite");
    materialize_manifest_view_database(&original, &blobs, &base).unwrap();
    let mut outputs = Vec::new();
    for incremental in [false, true] {
        let db = temp.join(if incremental {
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
    assert_snapshot_parity(&outputs[0], &outputs[1]);
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

#[test]
fn new_reexport_target_invalidates_transitive_unchanged_importer() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let barrel = "export * from './new';";
    let facade = "export * from './barrel';";
    let caller = "import { fresh } from './facade'; export function caller() { return fresh(); }";
    let base = manifest(
        &conn,
        &[
            ("barrel.ts", barrel),
            ("facade.ts", facade),
            ("caller.ts", caller),
            ("other.ts", "export function other() {}"),
        ],
    );
    let next = manifest(
        &conn,
        &[
            ("barrel.ts", barrel),
            ("facade.ts", facade),
            ("caller.ts", caller),
            ("other.ts", "export function other() {}"),
            ("new.ts", "export function fresh() { return 1; }"),
        ],
    );
    let db = f.dir.path().join("reexport.sqlite");
    let cold = f.dir.path().join("reexport-cold.sqlite");
    materialize_manifest_view_database(&db, &f.blobs, &base).unwrap();
    let stats = apply_manifest_diff(&db, &base, &next, &f.blobs).unwrap();
    materialize_manifest_view_database(&cold, &f.blobs, &next).unwrap();
    let actual = snapshot(&db);
    for (table, expected) in snapshot(&cold) {
        assert_eq!(actual[&table], expected, "table {table}");
    }
    assert_eq!(stats.dependent_files, 3);
    assert_eq!(stats.resolved_files, 4);
    assert!(!stats.full_resolution);
    assert_eq!(actual["edges"].len(), 1);
}

#[test]
fn resolver_configuration_names_force_full_resolution() {
    let f = fixture();
    for name in [
        "package.json",
        "tsconfig.json",
        "pnpm-workspace.yaml",
        "Cargo.toml",
    ] {
        let mut next = f.base.clone();
        next.insert(
            RelPath::new(name.as_bytes()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    callgraph: None,
                    semantic: None,
                },
                resolution_input: false,
            },
        )
        .unwrap();
        let db = f.dir.path().join(format!("config-{name}.sqlite"));
        materialize_manifest_view_database(&db, &f.blobs, &f.base).unwrap();
        assert!(
            apply_manifest_diff(&db, &f.base, &next, &f.blobs)
                .unwrap()
                .full_resolution,
            "{name}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "controlled 300-path offline write/CPU measurement"]
fn bench_controlled_300_path_diff() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let files = (0..5056).map(|i| (format!("file_{i}.ts"), format!("export function value_{i}() {{ return 1; }} export function caller_{i}() {{ return value_{i}(); }}"))).collect::<Vec<_>>();
    let next_files = files
        .iter()
        .enumerate()
        .map(|(i, (path, source))| {
            (
                path.clone(),
                if i < 300 {
                    format!("\n{source}")
                } else {
                    source.clone()
                },
            )
        })
        .collect::<Vec<_>>();
    let base = manifest(
        &conn,
        &files
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect::<Vec<_>>(),
    );
    let next = manifest(
        &conn,
        &next_files
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        base.entries()
            .filter(|(path, entry)| next.get(path) != Some(entry))
            .count(),
        300
    );
    let original = f.dir.path().join("controlled-base.sqlite");
    materialize_manifest_view_database(&original, &f.blobs, &base).unwrap();
    let mut outputs = Vec::new();
    for incremental in [false, true] {
        let db = f
            .dir
            .path()
            .join(format!("controlled-{incremental}.sqlite"));
        std::fs::copy(&original, &db).unwrap();
        let keeper = Connection::open(&db).unwrap();
        keeper
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let before = usage();
        let start = std::time::Instant::now();
        let stats = materialize(&db, &f.blobs, &next, incremental.then_some(&base)).unwrap();
        let elapsed = start.elapsed().as_secs_f64();
        let after = usage();
        let wal = std::fs::metadata(format!("{}-wal", db.display()))
            .unwrap()
            .len();
        println!("controlled 300/5056 incremental={incremental} wall_s={elapsed:.3} cpu_s={:.3} physical_bytes={} logical_bytes={} wal_bytes={wal} stats={stats:?}", after.2-before.2, after.0-before.0, after.1-before.1);
        outputs.push(snapshot(&db));
    }
    assert_eq!(outputs[0], outputs[1]);
}

#[test]
fn added_rust_module_invalidates_missing_candidate() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let lib = "mod future; pub fn caller() { future::fresh(); }";
    let base = manifest(&conn, &[("src/lib.rs", lib)]);
    let next = manifest(
        &conn,
        &[("src/lib.rs", lib), ("src/future.rs", "pub fn fresh() {}")],
    );
    let db = f.dir.path().join("rust.sqlite");
    let cold = f.dir.path().join("rust-cold.sqlite");
    materialize_manifest_view_database(&db, &f.blobs, &base).unwrap();
    let stats = apply_manifest_diff(&db, &base, &next, &f.blobs).unwrap();
    materialize_manifest_view_database(&cold, &f.blobs, &next).unwrap();
    assert_eq!(snapshot(&db), snapshot(&cold));
    assert_eq!(stats.dependent_files, 1);
    assert!(!stats.full_resolution);
}

#[test]
fn changed_tsconfig_relinks_unchanged_importer_with_cold_parity() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let mut base = manifest(
        &conn,
        &[
            (
                "caller.ts",
                "import { target } from '@lib'; export function caller() { return target(); }",
            ),
            ("one.ts", "export function target() {}"),
            ("two.ts", "export function target() {}"),
        ],
    );
    let set_config = |manifest: &mut Manifest, target: &str| {
        let source =
            format!(r#"{{"compilerOptions":{{"baseUrl":".","paths":{{"@lib":["{target}"]}}}}}}"#);
        let payload = join::CallgraphBlob::config(source.into_bytes(), "fixture")
            .to_bytes()
            .unwrap();
        let key = blake3::hash(&payload);
        conn.execute(
            "INSERT INTO blob_payloads VALUES (?1, ?2)",
            params![key.as_bytes().as_slice(), payload],
        )
        .unwrap();
        manifest
            .insert(
                RelPath::new(b"tsconfig.json".to_vec()).unwrap(),
                ManifestEntry::Regular {
                    mode: 0o100644,
                    planes: RegularPlanes {
                        callgraph: Some(key.to_hex().to_string()),
                        semantic: None,
                    },
                    resolution_input: true,
                },
            )
            .unwrap();
    };
    let mut next = base.clone();
    set_config(&mut base, "one.ts");
    set_config(&mut next, "two.ts");
    let db = f.dir.path().join("tsconfig.sqlite");
    let cold = f.dir.path().join("tsconfig-cold.sqlite");
    materialize_manifest_view_database(&db, &f.blobs, &base).unwrap();
    let stats = apply_manifest_diff(&db, &base, &next, &f.blobs).unwrap();
    materialize_manifest_view_database(&cold, &f.blobs, &next).unwrap();
    assert_eq!(snapshot(&db), snapshot(&cold));
    assert!(stats.full_resolution);
    let target: String = Connection::open(&db)
        .unwrap()
        .query_row("SELECT target_file FROM edges", [], |row| row.get(0))
        .unwrap();
    assert_eq!(target, "two.ts");
}

#[cfg(target_os = "macos")]
fn assert_snapshot_parity(
    expected: &BTreeMap<String, Vec<String>>,
    actual: &BTreeMap<String, Vec<String>>,
) {
    for (table, rows) in expected {
        if rows != &actual[table] {
            let expected = rows.iter().collect::<BTreeSet<_>>();
            let actual = actual[table].iter().collect::<BTreeSet<_>>();
            let missing = expected.difference(&actual).collect::<Vec<_>>();
            let extra = actual.difference(&expected).collect::<Vec<_>>();
            panic!(
                "table {table}: missing={} extra={}; first missing={:?}; first extra={:?}",
                missing.len(),
                extra.len(),
                missing
                    .first()
                    .map(|row| row.chars().take(1000).collect::<String>()),
                extra
                    .first()
                    .map(|row| row.chars().take(1000).collect::<String>())
            );
        }
    }
}

#[test]
fn binding_dependencies_exclude_existing_workspace_directory_probes() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let manifest = manifest(
        &conn,
        &[
            (
                "caller.ts",
                "import { target } from './dir'; export function caller() { return target(); }",
            ),
            ("dir/index.ts", "export function target() {}"),
        ],
    );
    let reader = ManifestViewBlobReader { connection: &conn };
    let cold = join::join_selected_manifest(&manifest, &reader, None, &BTreeMap::new()).unwrap();
    assert!(!cold.bindings["caller.ts"].dependencies.contains("dir"));
    assert!(cold.bindings["caller.ts"]
        .dependencies
        .contains("dir/index.ts"));
}

#[test]
fn legacy_generation_without_diff_metadata_cold_upgrades() {
    let f = fixture();
    let (_, copy) = prepare(&f);
    Connection::open(&copy).unwrap().execute_batch(
        "DELETE FROM meta WHERE k IN ('view_manifest_fingerprint', 'view_materialization_version');
         DROP TABLE view_bindings; DELETE FROM file_dependencies;"
    ).unwrap();
    let stats = apply_manifest_diff(&copy, &f.base, &f.next, &f.blobs).unwrap();
    assert!(stats.full_resolution);
    let cold = f.dir.path().join("legacy-upgraded-cold.sqlite");
    materialize_manifest_view_database(&cold, &f.blobs, &f.next).unwrap();
    assert_eq!(snapshot(&copy), snapshot(&cold));
}

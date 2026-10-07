use super::*;
use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};
use tempfile::TempDir;

#[test]
fn manifest_blob_reader_rejects_corrupt_digest_and_schema_at_consumption() {
    for column in ["payload_digest", "payload_schema"] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::blob_store::BlobStore::open(
            dir.path(),
            "integrity",
            crate::blob_store::BlobPlane::Callgraph,
        )
        .unwrap();
        let source = "export function target() {}";
        let key = crate::blob_store::CallgraphKey::for_current(source.as_bytes(), "typescript")
            .full_key();
        let payload =
            join::CallgraphBlob::extract(source, "typescript", crate::views::callgraph::PRODUCER)
                .unwrap()
                .to_bytes()
                .unwrap();
        store.put(&key, &payload).unwrap();
        let connection = Connection::open(store.path()).unwrap();
        let reader = ManifestViewBlobReader::new(&connection);
        assert_eq!(reader.read_payload(&key.to_hex()).unwrap(), Some(payload));
        let value = if column == "payload_digest" {
            "zeroblob(32)"
        } else {
            "999"
        };
        connection
            .execute(&format!("UPDATE blob_payloads SET {column} = {value}"), [])
            .unwrap();
        assert!(
            reader.read_payload(&key.to_hex()).unwrap().is_none(),
            "must reject {column} corruption before joining a graph"
        );
    }
}

thread_local! {
    // Offline paired measurements keep the old SQL lookup as an in-process control.
    pub(super) static PER_REFERENCE_LOOKUP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

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
                "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2, ?3, 1)",
                params![
                    key.as_bytes().as_slice(),
                    payload,
                    blake3::hash(&payload).as_bytes().as_slice()
                ],
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
        "CREATE TABLE blob_payloads(full_key BLOB PRIMARY KEY, payload BLOB NOT NULL, payload_digest BLOB NOT NULL, payload_schema INTEGER NOT NULL)",
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

/// Logical content of a derived database: schema rows plus sorted, typed rows
/// of every table. See [`parity`] for the definition.
fn snapshot(path: &Path) -> parity::LogicalSnapshot {
    parity::logical_snapshot(path)
}

fn prepare(f: &Fixture) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = f.dir.path().join("base.sqlite");
    let copy = f.dir.path().join("copy.sqlite");
    materialize_manifest_view_database(&base, &f.blobs, &f.base).unwrap();
    std::fs::copy(&base, &copy).unwrap();
    (base, copy)
}

#[test]
fn manifest_blob_reader_decodes_each_payload_once() {
    let f = fixture();
    let connection = Connection::open(&f.blobs).unwrap();
    let key = f
        .base
        .entries()
        .find_map(|(_, entry)| match entry {
            ManifestEntry::Regular { planes, .. } => planes.callgraph.as_deref(),
            _ => None,
        })
        .unwrap();
    let reader = ManifestViewBlobReader::new(&connection);

    let first = reader.read_decoded(key).unwrap().unwrap();
    let second = reader.read_decoded(key).unwrap().unwrap();

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(reader.decoded.borrow().len(), 1);
}

#[test]
fn selected_binding_load_skips_changed_payloads_and_matches_cold() {
    let f = fixture();
    let (_, incremental) = prepare(&f);
    Connection::open(&incremental)
        .unwrap()
        .execute(
            "UPDATE view_bindings SET payload = '{' WHERE file_path = 'target.ts'",
            [],
        )
        .unwrap();

    apply_manifest_diff(&incremental, &f.base, &f.next, &f.blobs).unwrap();
    let cold = f.dir.path().join("selected-binding-cold.sqlite");
    materialize_manifest_view_database(&cold, &f.blobs, &f.next).unwrap();
    assert_snapshot_parity(&snapshot(&incremental), &snapshot(&cold));
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
    assert_snapshot_parity(&snapshot(&cold), &actual);
    assert!(
        stats.relinked_inserted > 0,
        "fixture must exercise incoming edges"
    );
    assert!(stats.emission_lookup_queries > 0);
    assert!(
        stats.emission_lookup_queries <= stats.dependent_files,
        "existing refs must be loaded once per dependent caller, not once per reference: {stats:?}"
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
            delete_paths_touched: 3,
            deleted: 4,
            inserted: 5,
            relinked_deleted: 2,
            relinked_inserted: 2,
            dependency_deleted: 3,
            dependency_inserted: 3,
            surface_deleted: 2,
            surface_inserted: 2,
            dependent_files: 1,
            resolved_files: 3,
            resolved_refs: 2,
            resolved_bindings: 2,
            emission_lookup_queries: 1,
            rebuilt_surface_entries: 2,
            decoded_caller_blobs: 3,
            full_resolution: false,
            unattributed_callers: 0,
        }
    );
    assert_eq!(stats.graph_rows_written(), 13);
    assert_eq!(stats.rows_written(), 23);
    assert_eq!(stats.dependent_files, 1);
    assert_eq!(stats.resolved_files, 3);
    assert!(!stats.full_resolution);
}

#[test]
fn incremental_delete_rows_are_bounded_by_changed_manifest_paths() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let files = (0..32)
        .map(|i| {
            (
                format!("file_{i}.ts"),
                format!(
                    "export function value_{i}() {{ return 1; }} export function caller_{i}() {{ return value_{i}(); }}"
                ),
            )
        })
        .collect::<Vec<_>>();
    let next_files = files
        .iter()
        .enumerate()
        .map(|(i, (path, source))| {
            (
                path.clone(),
                if i < 3 {
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
    let database = f.dir.path().join("bounded-delete.sqlite");
    materialize_manifest_view_database(&database, &f.blobs, &base).unwrap();
    let stats = apply_manifest_diff(&database, &base, &next, &f.blobs).unwrap();
    let changed_files = 3;

    println!(
        "bounded deletion: changed_files={changed_files} paths_touched={} graph_rows_deleted={} dependency_rows_deleted={} surface_rows_deleted={}",
        stats.delete_paths_touched,
        stats.deleted,
        stats.dependency_deleted,
        stats.surface_deleted
    );
    assert_eq!(stats.delete_paths_touched, changed_files);
    assert_eq!(stats.deleted, changed_files * 5);
    assert_eq!(stats.dependency_deleted, changed_files * 2);
    assert_eq!(stats.surface_deleted, changed_files);
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
fn incremental_phase_table_requires_every_phase_timed() {
    let f = fixture();
    let (_, copy) = prepare(&f);
    let (_, timings) = apply_manifest_diff_profiled(&copy, &f.base, &f.next, &f.blobs).unwrap();
    let table = profile::offline_phase_table(
        profile::PhaseMeasurement::observed(1, 0),
        &timings,
        profile::PhaseMeasurement::observed(1, 1),
    )
    .expect("phase table must reject an untimed materialization bucket");
    for phase in [
        "clone",
        "selected_join",
        "delete_rows",
        "emit_files",
        "emit_nodes",
        "emit_view_file_surfaces",
        "emit_file_dependencies",
        "emit_view_bindings",
        "emit_refs",
        "emit_edges",
        "index_maintenance",
        "checkpoint",
    ] {
        assert!(table.contains(&format!("| {phase} |")), "missing {phase}");
    }
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
    assert_eq!(
        fingerprint(&base).unwrap(),
        "ffc2ece68208cd454853233d2bad29627131be253c78da5a6644cfcd3d8c3ffb",
        "offline benchmark must use the retained opencode HEAD manifest"
    );
    assert_eq!(
        fingerprint(&next).unwrap(),
        "e0b49e85a7b8674087a9026489c830ae954f82a337299a2ea9d6505e96955a29",
        "offline benchmark must use the retained 300-Git-path target manifest"
    );
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
    let cold = temp.join("cold.sqlite");
    materialize_manifest_view_database(&cold, &blobs, &next).unwrap();
    let cold_snapshot = snapshot(&cold);
    let repetitions = std::env::var("AFT_VIEW_BENCH_REPETITIONS")
        .map(|value| value.parse::<usize>().expect("benchmark repetitions"))
        .unwrap_or(1);

    for run in 1..=repetitions {
        let db = temp.join(format!("incremental-{run}.sqlite"));
        let clone_started = std::time::Instant::now();
        crate::views::generation::clone_derived(&original, &db).unwrap();
        let clone = profile::PhaseMeasurement::observed(clone_started.elapsed().as_nanos(), 0);
        let keeper = Connection::open(&db).unwrap();
        keeper
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let before = usage();
        let mut load_before = [0.0; 3];
        // Sample host load at the measured call, not before compilation.
        unsafe {
            libc::getloadavg(load_before.as_mut_ptr(), 3);
        }
        let start = std::time::Instant::now();
        let (stats, timings) = materialize(&db, &blobs, &next, Some(&base)).unwrap();
        let elapsed = start.elapsed().as_secs_f64();
        let after = usage();
        let mut load_after = [0.0; 3];
        unsafe {
            libc::getloadavg(load_after.as_mut_ptr(), 3);
        }
        let wal_path = format!("{}-wal", db.display());
        let wal = std::fs::metadata(&wal_path).unwrap().len();
        report_wal_pages(&db);
        let checkpoint_started = std::time::Instant::now();
        let (_, _, checkpointed): (i64, i64, i64) = keeper
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        keeper
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let checkpoint = profile::PhaseMeasurement::observed(
            checkpoint_started.elapsed().as_nanos(),
            checkpointed.max(0) as u64,
        );
        println!("offline incremental phase table run={run}");
        let table = profile::offline_phase_table(clone, &timings, checkpoint)
            .expect("every offline materialization phase must be timed");
        println!("{table}");
        if timings
            .writes
            .get(profile::WritePhase::IndexMaintenance)
            .wall_ns
            == 0
        {
            println!("index_maintenance is inline and charged to delete/emission rows");
        }
        println!("load_before={load_before:?} load_after={load_after:?}");
        println!("incremental=true run={run} wall_s={elapsed:.3} cpu_s={:.3} physical_bytes={} logical_bytes={} wal_bytes={wal} stats={stats:?}", after.2-before.2, after.0-before.0, after.1-before.1);
        assert_snapshot_parity(&cold_snapshot, &snapshot(&db));
    }
}

/// Calibrates the incremental-versus-cold cutoff in `views::assembly`.
///
/// Clones `AFT_VIEW_CUTOFF_SOURCE` with `git clone --shared` into a fresh
/// directory (the source checkout is only read), publishes
/// `AFT_VIEW_CUTOFF_BASE` and every comma-separated revision in
/// `AFT_VIEW_CUTOFF_TARGETS` into isolated view storage to obtain real
/// manifests and blobs, then times, per target, a cold build into a new file
/// against a clone of the cold base plus `apply_manifest_diff`. Both paths
/// keep a keeper connection open, as publication does, so neither pays a
/// checkpoint on close.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "offline cutoff calibration; builds its own clone and isolated view storage"]
fn bench_incremental_cutoff_crossover() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    let source = var("AFT_VIEW_CUTOFF_SOURCE");
    let base_revision = var("AFT_VIEW_CUTOFF_BASE");
    let targets = std::env::var("AFT_VIEW_CUTOFF_TARGETS").unwrap_or_default();
    let repetitions = std::env::var("AFT_VIEW_BENCH_REPETITIONS")
        .map(|value| value.parse::<usize>().expect("benchmark repetitions"))
        .unwrap_or(1);
    let out = tempfile::tempdir_in(
        std::env::var_os("AFT_VIEW_CUTOFF_OUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir),
    )
    .unwrap();
    println!("cutoff calibration directory: {}", out.path().display());
    let checkout = out.path().join("checkout");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&[
        "clone",
        "--shared",
        "--quiet",
        "--no-checkout",
        &source,
        checkout.to_str().unwrap(),
    ]);
    let storage = out.path().join("storage");
    let publish = |revision: &str| -> Manifest {
        git(&[
            "-C",
            checkout.to_str().unwrap(),
            "checkout",
            "--quiet",
            "--force",
            revision,
        ]);
        let started = std::time::Instant::now();
        let report =
            crate::views::assembly::publish_checkout(&crate::views::assembly::AssemblyRequest {
                storage: storage.clone(),
                project_root: checkout.clone(),
                family: "cutoff".into(),
                scope: "cutoff".into(),
                desired_head: revision.into(),
                changed_paths: BTreeSet::new(),
                semantic_keys: Default::default(),
                require_semantic: false,
                allow_blob_put: true,
                callgraph: true,
            })
            .unwrap();
        println!(
            "published {revision} in {:.1} s",
            started.elapsed().as_secs_f64()
        );
        report
            .manifest
            .expect("distinct revisions publish a manifest")
    };
    let base = publish(&base_revision);
    let mut targets = targets
        .split(',')
        .filter(|revision| !revision.is_empty())
        .map(|revision| (revision.to_string(), publish(revision)))
        .collect::<Vec<_>>();
    // Synthetic targets edit a share of the base's TypeScript files and change
    // no resolver configuration, so they measure the pruned incremental path
    // at diff sizes where real history also changed configuration.
    for percent in std::env::var("AFT_VIEW_CUTOFF_SYNTHETIC")
        .unwrap_or_default()
        .split(',')
        .filter(|value| !value.is_empty())
    {
        let percent = percent.parse::<usize>().expect("synthetic percent");
        let candidates = base
            .entries()
            .filter_map(|(path, entry)| {
                let path = std::str::from_utf8(path.as_bytes()).ok()?;
                let typescript = path.ends_with(".ts") || path.ends_with(".tsx");
                matches!(entry, ManifestEntry::Regular { planes, resolution_input: false, .. } if planes.callgraph.is_some())
                    .then_some(path.to_string())
                    .filter(|_| typescript)
            })
            .collect::<Vec<_>>();
        let wanted = base.entries().count() * percent / 100;
        assert!(
            wanted <= candidates.len(),
            "{percent}% needs {wanted} files; only {} TypeScript files",
            candidates.len()
        );
        git(&[
            "-C",
            checkout.to_str().unwrap(),
            "checkout",
            "--quiet",
            "--force",
            &base_revision,
        ]);
        // Spread the edits across the tree instead of taking one directory.
        for index in 0..wanted {
            let path = checkout.join(&candidates[index * candidates.len() / wanted]);
            let source = std::fs::read_to_string(&path).unwrap_or_default();
            std::fs::write(
                &path,
                format!("export function cutoff_probe_{index}() {{}}\n{source}"),
            )
            .unwrap();
        }
        let label = format!("synthetic-{percent}pct");
        let report =
            crate::views::assembly::publish_checkout(&crate::views::assembly::AssemblyRequest {
                storage: storage.clone(),
                project_root: checkout.clone(),
                family: "cutoff".into(),
                scope: "cutoff".into(),
                desired_head: label.clone(),
                changed_paths: BTreeSet::new(),
                semantic_keys: Default::default(),
                require_semantic: false,
                allow_blob_put: true,
                callgraph: true,
            })
            .unwrap();
        targets.push((label, report.manifest.expect("synthetic manifest")));
    }
    let blobs = crate::blob_store::BlobStore::open(
        &storage,
        "cutoff".to_string(),
        crate::blob_store::BlobPlane::Callgraph,
    )
    .unwrap()
    .path()
    .to_path_buf();
    let keeper = |path: &Path| {
        let keeper =
            crate::db::file_identity::IdentityConnection::open(path, "cutoff calibration keeper")
                .unwrap();
        keeper.pragma_update(None, "journal_mode", "WAL").unwrap();
        keeper
            .query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        keeper
    };
    let remove = |path: &Path| {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    };
    let wal_bytes = |path: &Path| {
        std::fs::metadata(format!("{}-wal", path.display()))
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    };
    let base_db = out.path().join("base.sqlite");
    materialize_manifest_view_database(&base_db, &blobs, &base).unwrap();
    let base_bytes = std::fs::metadata(&base_db).unwrap().len();
    println!(
        "base entries={} derived_bytes={base_bytes}",
        base.entries().count()
    );
    for (index, (revision, target)) in targets.iter().enumerate() {
        let size = manifest_diff_size(&base, target);
        let callgraph_changed = base
            .entries()
            .chain(target.entries())
            .filter(|(path, _)| base.get(path) != target.get(path))
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| {
                let key = |manifest: &Manifest| match manifest.get(path) {
                    Some(ManifestEntry::Regular { planes, .. }) => planes.callgraph.clone(),
                    _ => None,
                };
                key(&base).is_some() || key(target).is_some()
            })
            .count();
        let changed_paths = base
            .entries()
            .chain(target.entries())
            .filter(|(path, _)| base.get(path) != target.get(path))
            .map(|(path, _)| path.as_bytes().to_vec())
            .collect::<BTreeSet<_>>();
        let special_entry_changed = requires_full_resolution(&base, target, &changed_paths);
        let configuration_changed = changed_paths
            .iter()
            .any(|path| join::view_resolution_config(path));
        for run in 1..=repetitions {
            let mut load = [0.0; 3];
            unsafe {
                libc::getloadavg(load.as_mut_ptr(), 3);
            }
            let cold = out.path().join(format!("cold-{index}-{run}.sqlite"));
            let cold_keeper = keeper(&cold);
            let before = usage();
            let started = std::time::Instant::now();
            materialize_manifest_view_database(&cold, &blobs, target).unwrap();
            let cold_s = started.elapsed().as_secs_f64();
            let cold_cpu = usage().2 - before.2;
            let cold_wal = wal_bytes(&cold);

            let incremental = out.path().join(format!("incremental-{index}-{run}.sqlite"));
            let before = usage();
            let started = std::time::Instant::now();
            crate::views::generation::clone_derived(&base_db, &incremental).unwrap();
            let clone_s = started.elapsed().as_secs_f64();
            let incremental_keeper = keeper(&incremental);
            let started = std::time::Instant::now();
            let (stats, _) = materialize(&incremental, &blobs, target, Some(&base)).unwrap();
            let patch_s = started.elapsed().as_secs_f64();
            let incremental_cpu = usage().2 - before.2;
            let incremental_wal = wal_bytes(&incremental);
            println!(
                "cutoff target={revision} run={run} changed={} callgraph_changed={callgraph_changed} entries={} changed_pct={:.1} cold_s={cold_s:.2} clone_s={clone_s:.2} patch_s={patch_s:.2} incremental_s={:.2} cold_cpu_s={cold_cpu:.2} incremental_cpu_s={incremental_cpu:.2} cold_wal={cold_wal} incremental_wal={incremental_wal} full_resolution={} special_entry_changed={special_entry_changed} configuration_changed={configuration_changed} may_force_full_resolution={} cutoff_decision={} resolved_files={} load={:.1}",
                size.changed,
                size.entries,
                size.changed as f64 * 100.0 / size.entries as f64,
                clone_s + patch_s,
                stats.full_resolution,
                size.may_force_full_resolution,
                if crate::views::assembly::diff_exceeds_incremental_cutoff(size) {
                    "cold"
                } else {
                    "incremental"
                },
                stats.resolved_files,
                load[0],
            );
            drop(cold_keeper);
            drop(incremental_keeper);
            if run == 1 && std::env::var_os("AFT_VIEW_CUTOFF_SKIP_PARITY").is_none() {
                assert_snapshot_parity(&snapshot(&cold), &snapshot(&incremental));
            }
            remove(&cold);
            remove(&incremental);
        }
    }
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
    let reader = ManifestViewBlobReader::new(&conn);
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
    assert_snapshot_parity(&snapshot(&cold), &actual);
    assert_eq!(stats.dependent_files, 1);
    assert_eq!(stats.resolved_files, 2);
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
            "INSERT INTO blob_payloads VALUES (?1, ?2, ?3, 1)",
            params![
                key.as_bytes().as_slice(),
                payload,
                blake3::hash(&payload).as_bytes().as_slice()
            ],
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
    assert!(!stats.full_resolution);
    assert_eq!(stats.dependent_files, 1);
    let target: String = Connection::open(&db)
        .unwrap()
        .query_row("SELECT target_file FROM edges", [], |row| row.get(0))
        .unwrap();
    assert_eq!(target, "two.ts");
}

/// Compares schema and every table in either snapshot, so an extra table,
/// a missing or different index, or a changed column type is a failure.
fn assert_snapshot_parity(expected: &parity::LogicalSnapshot, actual: &parity::LogicalSnapshot) {
    parity::assert_snapshots_equal(expected, actual);
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
    let reader = ManifestViewBlobReader::new(&conn);
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

#[test]
fn unrelated_export_surface_change_skips_reresolving_unchanged_binding_caller() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let caller = "import { target } from './target'; export function caller() { return target(); }";
    let base = manifest(
        &conn,
        &[
            ("caller.ts", caller),
            (
                "target.ts",
                "const anchor = 0; export function target() { return 1; }",
            ),
        ],
    );
    let next = manifest(
        &conn,
        &[
            ("caller.ts", caller),
            (
                "target.ts",
                "const anchor = 0; export function target() { return 1; } export function unrelated() {}",
            ),
        ],
    );
    let db = f.dir.path().join("unrelated-surface.sqlite");
    let cold = f.dir.path().join("unrelated-surface-cold.sqlite");
    materialize_manifest_view_database(&db, &f.blobs, &base).unwrap();
    let stats = apply_manifest_diff(&db, &base, &next, &f.blobs).unwrap();
    materialize_manifest_view_database(&cold, &f.blobs, &next).unwrap();
    assert_eq!(snapshot(&db), snapshot(&cold));
    assert_eq!(stats.dependent_files, 0);
    assert_eq!(stats.resolved_refs, 0);
}

#[test]
fn used_export_surface_change_reresolves_unchanged_binding_caller() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let caller = "import { target } from './target'; export function caller() { return target(); }";
    let base = manifest(
        &conn,
        &[
            ("caller.ts", caller),
            ("target.ts", "export function target() { return 1; }"),
        ],
    );
    let next = manifest(
        &conn,
        &[
            ("caller.ts", caller),
            ("target.ts", "export function replacement() { return 1; }"),
        ],
    );
    let db = f.dir.path().join("used-surface.sqlite");
    let cold = f.dir.path().join("used-surface-cold.sqlite");
    materialize_manifest_view_database(&db, &f.blobs, &base).unwrap();
    let stats = apply_manifest_diff(&db, &base, &next, &f.blobs).unwrap();
    materialize_manifest_view_database(&cold, &f.blobs, &next).unwrap();
    let actual = snapshot(&db);
    assert_snapshot_parity(&snapshot(&cold), &actual);
    assert_eq!(stats.dependent_files, 1);
    assert_eq!(stats.resolved_refs, 2);
}

#[test]
fn colliding_structural_ordinals_keep_distinct_bindings_and_first_reference_rows() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let initial = manifest(
        &conn,
        &[
            (
                "caller.ts",
                "import { one } from './barrel'; export function caller() { return one(); }",
            ),
            ("barrel.ts", "export * from './one'; export * from './two';"),
            ("one.ts", "export function one() {}"),
            ("two.ts", "export function two() {}"),
        ],
    );
    let barrel = RelPath::new(b"barrel.ts".to_vec()).unwrap();
    let ManifestEntry::Regular { planes, .. } = initial.get(&barrel).unwrap() else {
        unreachable!()
    };
    let payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM blob_payloads WHERE full_key=?1",
            [decode_manifest_full_key(planes.callgraph.as_deref().unwrap()).unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    let mut blob = join::CallgraphBlob::from_bytes(&payload).unwrap();
    let join::CallgraphBlob::Parse(parse) = &mut blob else {
        unreachable!()
    };
    let reexports = parse
        .refs
        .iter()
        .enumerate()
        .filter(|(_, reference)| reference.kind == join::BlobRefKind::Reexport)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(reexports.len(), 2);
    parse.refs[reexports[1]].ordinal = parse.refs[reexports[0]].ordinal;
    let first_module = parse.refs[reexports[0]].module_path.clone().unwrap();
    let payload = blob.to_bytes().unwrap();
    let key = blake3::hash(&payload);
    conn.execute(
        "INSERT INTO blob_payloads VALUES(?1, ?2, ?3, 1)",
        params![
            key.as_bytes().as_slice(),
            payload,
            blake3::hash(&payload).as_bytes().as_slice()
        ],
    )
    .unwrap();
    let manifest = Manifest::new(initial.entries().map(|(path, entry)| {
        let mut entry = entry.clone();
        if path == &barrel {
            if let ManifestEntry::Regular { planes, .. } = &mut entry {
                planes.callgraph = Some(key.to_hex().to_string());
            }
        }
        (path.clone(), entry)
    }))
    .unwrap();
    let reader = ManifestViewBlobReader::new(&conn);
    let cold = join::join_selected_manifest(&manifest, &reader, None, &BTreeMap::new()).unwrap();
    let selected = BTreeSet::from(["caller.ts".to_string()]);
    let cached =
        join::join_selected_manifest(&manifest, &reader, Some(&selected), &cold.bindings).unwrap();
    assert_eq!(
        cached.result.rows,
        cold.result
            .rows
            .into_iter()
            .filter(|row| row.caller_path == b"caller.ts")
            .collect()
    );
    let db = f.dir.path().join("colliding.sqlite");
    materialize_manifest_view_database(&db, &f.blobs, &manifest).unwrap();
    let module: String = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT module_path FROM refs WHERE caller_file='barrel.ts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(module, first_module);
}

#[cfg(target_os = "macos")]
fn report_wal_pages(db: &std::path::Path) {
    let connection = Connection::open(db).unwrap();
    let page_size: usize = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    let mut statement = connection
        .prepare("SELECT pageno, name FROM dbstat")
        .unwrap();
    let owners = statement
        .query_map([], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .unwrap();
    let bytes = std::fs::read(format!("{}-wal", db.display())).unwrap();
    let mut frames = BTreeMap::<String, usize>::new();
    for frame in bytes[32..].chunks_exact(page_size + 24) {
        let page = u32::from_be_bytes(frame[..4].try_into().unwrap());
        *frames
            .entry(
                owners
                    .get(&page)
                    .cloned()
                    .unwrap_or_else(|| "<freelist-or-unmapped>".into()),
            )
            .or_default() += 1;
    }
    // Ownership is the final dbstat mapping; reused pages may have had another
    // owner earlier in the transaction. Count every frame, not distinct pages.
    println!(
        "wal_frame_bytes_by_final_owner={:?}",
        frames
            .into_iter()
            .map(|(name, frames)| (name, frames * (page_size + 24)))
            .collect::<BTreeMap<_, _>>()
    );
}

#[test]
fn persistent_surfaces_rebuild_only_changed_entries_without_reading_pruned_callers() {
    let f = fixture();
    let connection = Connection::open(&f.blobs).unwrap();
    let caller = "import { target } from './target'; export function caller() { return target(); }";
    let base = manifest(
        &connection,
        &[
            ("caller.ts", caller),
            (
                "target.ts",
                "const anchor = 0; export function target() { return 1; }",
            ),
        ],
    );
    let next = manifest(
        &connection,
        &[
            ("caller.ts", caller),
            (
                "target.ts",
                "const anchor = 0; export function target() { return 1; } export function unrelated() {}",
            ),
        ],
    );
    let db = f.dir.path().join("persistent-surface.sqlite");
    materialize_manifest_view_database(&db, &f.blobs, &base).unwrap();
    let cache = load_bindings(&Connection::open(&db).unwrap()).unwrap();
    struct CountingReader<'a> {
        inner: ManifestViewBlobReader<'a>,
        reads: std::cell::RefCell<Vec<String>>,
    }
    impl join::ManifestBlobReader for CountingReader<'_> {
        fn read_callgraph_blob(
            &self,
            key: &str,
        ) -> std::result::Result<Option<Vec<u8>>, join::ManifestJoinError> {
            self.reads.borrow_mut().push(key.into());
            self.inner.read_callgraph_blob(key)
        }
    }
    let reader = CountingReader {
        inner: ManifestViewBlobReader::new(&connection),
        reads: Default::default(),
    };
    let selected = BTreeSet::from(["caller.ts".into(), "target.ts".into()]);
    let changed = BTreeSet::from(["target.ts".into()]);
    let joined = join::join_selected_manifest_reusing_surfaces(
        &next,
        &reader,
        Some(&selected),
        &cache,
        &changed,
        &BTreeSet::new(),
        &BTreeSet::new(),
    )
    .unwrap();
    assert_eq!(joined.rebuilt_surface_entries, 1);
    assert_eq!(joined.decoded_caller_blobs, 1);
    assert_eq!(
        reader.reads.borrow().len(),
        1,
        "only the changed target's immutable blob may be read"
    );
    assert_eq!(joined.resolved_callers, changed);
    let cold = f.dir.path().join("persistent-surface-cold.sqlite");
    apply_manifest_diff(&db, &base, &next, &f.blobs).unwrap();
    materialize_manifest_view_database(&cold, &f.blobs, &next).unwrap();
    assert_snapshot_parity(&snapshot(&db), &snapshot(&cold));
}

#[path = "fact_tests.rs"]
mod fact_tests;

#[test]
fn memoized_bindings_keep_callers_and_reference_kinds_distinct() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let source = "import { target } from './target'; export function caller() { target(); target(); return target; }";
    let current = manifest(
        &conn,
        &[
            ("a/caller.ts", source),
            ("b/caller.ts", source),
            ("a/target.ts", "export function target() {}"),
            ("b/target.ts", "export function target() {}"),
        ],
    );
    let reader = ManifestViewBlobReader::new(&conn);
    let reference = join::JoinResult::from_manifest(&current, &reader).unwrap();
    let memoized = join::join_selected_manifest(&current, &reader, None, &BTreeMap::new()).unwrap();
    assert_eq!(
        reference.canonical_serialization(),
        memoized.result.canonical_serialization()
    );
    assert_eq!(memoized.resolved_bindings, 4);
    assert!(memoized.resolved_bindings < memoized.result.resolution_order.len());
    for prefix in ["a", "b"] {
        assert!(memoized.result.rows.iter().any(|row| row.caller_path
            == format!("{prefix}/caller.ts").as_bytes()
            && row.target_path.as_deref() == Some(format!("{prefix}/target.ts").as_bytes())));
    }
}

#[test]
fn memoized_rust_bindings_keep_import_visibility() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let current = manifest(&conn, &[
        ("src/lib.rs", "mod target; fn before() { alias::target(); } use crate::target as alias; fn after() { alias::target(); alias::target(); }"),
        ("src/target.rs", "pub fn target() {}"),
    ]);
    let reader = ManifestViewBlobReader::new(&conn);
    let reference = join::JoinResult::from_manifest(&current, &reader).unwrap();
    let memoized = join::join_selected_manifest(&current, &reader, None, &BTreeMap::new()).unwrap();
    assert_eq!(
        reference.canonical_serialization(),
        memoized.result.canonical_serialization()
    );
    assert!(memoized.resolved_bindings < memoized.result.resolution_order.len());
}

#[test]
fn memoized_bindings_keep_call_and_value_ref_results_distinct() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let current = manifest(
        &conn,
        &[(
            "caller.rs",
            "struct Foo; fn caller(value: Foo) { Foo(); let _constructor = Foo; let _ = value; }",
        )],
    );
    let reader = ManifestViewBlobReader::new(&conn);
    let reference = join::JoinResult::from_manifest(&current, &reader).unwrap();
    let memoized = join::join_selected_manifest(&current, &reader, None, &BTreeMap::new()).unwrap();

    assert_eq!(
        reference.canonical_serialization(),
        memoized.result.canonical_serialization()
    );
    let foo_rows = memoized
        .result
        .rows
        .iter()
        .filter(|row| {
            row.target_symbol.as_deref() == Some("Foo")
                || row.status == join::ResolutionStatus::Unresolved
        })
        .collect::<Vec<_>>();
    assert!(foo_rows.iter().any(|row| {
        row.kind == join::BlobRefKind::Call && row.status == join::ResolutionStatus::Resolved
    }));
    assert!(foo_rows.iter().any(|row| {
        row.kind == join::BlobRefKind::ValueRef && row.status == join::ResolutionStatus::Unresolved
    }));
}

/// A deterministic mixed TypeScript and Rust corpus that a seeded sequence of
/// edits walks through. Each version is a full path-to-source map.
struct DifferentialCorpus {
    files: BTreeMap<String, String>,
    next_id: usize,
    state: u64,
}

impl DifferentialCorpus {
    fn new(seed: u64) -> Self {
        let mut files = BTreeMap::new();
        for index in 0..4 {
            files.insert(
                format!("mod_{index}.ts"),
                format!("export function target_{index}() {{ return {index}; }}\n"),
            );
        }
        files.insert(
            "barrel.ts".into(),
            "export * from './mod_0';\nexport * from './mod_1';\n".into(),
        );
        files.insert(
            "caller.ts".into(),
            "import { target_0, target_1 } from './barrel';\n\
             import * as two from './mod_2';\n\
             export default function main() { return target_0() + target_1() + two.target_2(); }\n"
                .into(),
        );
        files.insert(
            "uses_default.ts".into(),
            "import main from './caller';\nexport function wrapper() { return main(); }\n".into(),
        );
        files.insert(
            "src/lib.rs".into(),
            "mod util;\npub use util::helper;\npub fn entry() { helper(); }\n".into(),
        );
        files.insert("src/util.rs".into(), "pub fn helper() {}\n".into());
        Self {
            files,
            next_id: 100,
            // Never zero, so the generator does not get stuck.
            state: seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1,
        }
    }

    fn next(&mut self, bound: usize) -> usize {
        // xorshift64: small, deterministic, and good enough to pick edits.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        (self.state % bound as u64) as usize
    }

    fn typescript_modules(&self) -> Vec<String> {
        self.files
            .keys()
            .filter(|path| path.starts_with("mod_"))
            .cloned()
            .collect()
    }

    /// Apply one randomly chosen edit and describe it.
    fn step(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        let modules = self.typescript_modules();
        match self.next(7) {
            0 => {
                // Insert a symbol before the others, renumbering every ordinal
                // after it, so incoming references must be relinked.
                let path = &modules[self.next(modules.len())];
                let source = self.files.get_mut(path).unwrap();
                source.insert_str(
                    0,
                    &format!("export function inserted_{id}() {{ return {id}; }}\n"),
                );
                format!("insert symbol in {path}")
            }
            1 => {
                let path = &modules[self.next(modules.len())];
                let source = self.files.get_mut(path).unwrap();
                source.push_str(&format!("// edit {id}\n"));
                format!("edit body of {path}")
            }
            2 => {
                let target = modules[self.next(modules.len())]
                    .trim_end_matches(".ts")
                    .to_string();
                let name = target.trim_start_matches("mod_").to_string();
                self.files.insert(
                    format!("mod_{id}.ts"),
                    format!(
                        "import {{ target_{name} }} from './{target}';\n\
                         export function target_{id}() {{ return target_{name}(); }}\n"
                    ),
                );
                format!("add mod_{id}.ts calling {target}")
            }
            3 if modules.len() > 2 => {
                let path = modules[self.next(modules.len())].clone();
                self.files.remove(&path);
                format!("remove {path}")
            }
            4 => {
                let path = &modules[self.next(modules.len())];
                let line = format!("export * from './{}';\n", path.trim_end_matches(".ts"));
                let barrel = self.files.get_mut("barrel.ts").unwrap();
                if barrel.contains(&line) {
                    *barrel = barrel.replace(&line, "");
                    format!("drop re-export of {path}")
                } else {
                    barrel.push_str(&line);
                    format!("add re-export of {path}")
                }
            }
            5 => {
                let module = format!("extra_{id}");
                self.files.insert(
                    format!("src/{module}.rs"),
                    format!("pub fn fresh_{id}() {{}}\n"),
                );
                let lib = self.files.get_mut("src/lib.rs").unwrap();
                lib.insert_str(0, &format!("mod {module};\n"));
                lib.push_str(&format!(
                    "pub fn use_{id}() {{ {module}::fresh_{id}(); }}\n"
                ));
                format!("add rust module {module}")
            }
            _ => {
                // Rename a target: callers that imported the old name stop
                // resolving and must be relinked as unresolved.
                let path = &modules[self.next(modules.len())];
                let source = self.files.get_mut(path).unwrap();
                *source = source.replacen("export function target_", "export function renamed_", 1);
                format!("rename first target in {path}")
            }
        }
    }
}

/// Incremental materialization equals a cold build for adjacent steps, for a
/// chain of steps applied to one database, and for non-adjacent pairs in both
/// directions (which include switching back to an earlier version).
#[test]
fn differential_incremental_matches_cold_for_adjacent_and_non_adjacent_diffs() {
    let f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    for seed in 1..=6_u64 {
        let mut corpus = DifferentialCorpus::new(seed);
        let mut versions = Vec::new();
        let mut steps = Vec::new();
        for step in 0..5 {
            if step > 0 {
                steps.push(corpus.step());
            }
            let files = corpus
                .files
                .iter()
                .map(|(path, source)| (path.as_str(), source.as_str()))
                .collect::<Vec<_>>();
            versions.push(manifest(&conn, &files));
        }
        let context = format!("seed={seed} steps={steps:?}");
        let cold = versions
            .iter()
            .enumerate()
            .map(|(index, version)| {
                let path = f
                    .dir
                    .path()
                    .join(format!("differential-{seed}-cold-{index}.sqlite"));
                materialize_manifest_view_database(&path, &f.blobs, version).unwrap();
                snapshot(&path)
            })
            .collect::<Vec<_>>();
        let check = |from: usize, to: usize, label: &str| {
            let path = f
                .dir
                .path()
                .join(format!("differential-{seed}-{label}-{from}-{to}.sqlite"));
            materialize_manifest_view_database(&path, &f.blobs, &versions[from]).unwrap();
            let stats = apply_manifest_diff(&path, &versions[from], &versions[to], &f.blobs)
                .unwrap_or_else(|error| panic!("{context} {from}->{to}: {error}"));
            assert!(!stats.full_resolution, "{context} {from}->{to}: {stats:?}");
            let differences = parity::snapshot_differences(&cold[to], &snapshot(&path));
            assert!(
                differences.is_empty(),
                "{context} {label} {from}->{to}:\n{}",
                differences.join("\n")
            );
        };
        for from in 0..versions.len() {
            for to in 0..versions.len() {
                match from.abs_diff(to) {
                    0 => {}
                    1 if to > from => check(from, to, "adjacent"),
                    1 => check(from, to, "adjacent-back"),
                    _ => check(from, to, "non-adjacent"),
                }
            }
        }
        // Chained: every step patches the previous step's output, so an error
        // in one step would seed a wrong base for the next.
        let chained = f
            .dir
            .path()
            .join(format!("differential-{seed}-chained.sqlite"));
        materialize_manifest_view_database(&chained, &f.blobs, &versions[0]).unwrap();
        for step in 1..versions.len() {
            apply_manifest_diff(&chained, &versions[step - 1], &versions[step], &f.blobs).unwrap();
            let differences = parity::snapshot_differences(&cold[step], &snapshot(&chained));
            assert!(
                differences.is_empty(),
                "{context} chained step {step}:\n{}",
                differences.join("\n")
            );
        }
    }
}

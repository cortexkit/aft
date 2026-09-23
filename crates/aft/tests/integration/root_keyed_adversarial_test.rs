use aft::callgraph::walk_project_files;
use aft::callgraph_store::{CallGraphRead, CallGraphStore};
use aft::config::Config;
use aft::context::AppContext;
use aft::harness::Harness;
use aft::parser::TreeSitterProvider;
use rusqlite::Connection;
use serde_json::json;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime};
use tempfile::tempdir;

const HELPER_TEST_NAME: &str = "root_keyed_adversarial_test::root_keyed_adversarial_child";
const SQLITE_FILE_SET_SUFFIXES: &[&str] = &["", "-wal", "-shm", "-journal"];

#[test]
fn root_keyed_two_processes_one_checkout_readonly_loser_reclaims_after_kill() {
    let dir = tempdir().unwrap();
    let root = canonical_temp_root(dir.path());
    write_project(&root, "leaf");
    let store_dir = root.join("store");
    let (initial, _) = CallGraphStore::cold_build_with_lease(
        store_dir.clone(),
        root.clone(),
        &project_files(&root),
    )
    .unwrap();
    assert_eq!(entry_leaf(&initial), "leaf");
    drop(initial);

    let ready = root.join("hold-writer.ready");
    let mut child = spawn_adversarial_helper("hold-writer", &root, &store_dir, &ready);
    wait_for_helper_ready(&mut child, &ready, Duration::from_secs(20));

    let lease: serde_json::Value =
        serde_json::from_slice(&fs::read(store_dir.join("writer.lease")).unwrap()).unwrap();
    assert_eq!(lease["pid"].as_u64(), Some(child.id() as u64));

    let loser_reader = CallGraphStore::open_readonly(store_dir.clone(), root.clone())
        .unwrap()
        .expect("the non-writer process must serve the published generation read-only");
    assert_eq!(entry_leaf(&loser_reader), "leaf");
    drop(loser_reader);

    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "killed writer unexpectedly exited cleanly"
    );

    let winner = CallGraphStore::open(store_dir.clone(), root.clone())
        .expect("the next writer open after the dead owner is reaped should acquire the lease");
    assert_eq!(entry_leaf(&winner), "leaf");
    let lease: serde_json::Value =
        serde_json::from_slice(&fs::read(store_dir.join("writer.lease")).unwrap()).unwrap();
    assert_eq!(lease["pid"].as_u64(), Some(std::process::id() as u64));
}

#[test]
fn root_keyed_lease_reclaim_race_rejects_stale_publish_with_fence_error() {
    let dir = tempdir().unwrap();
    let root = canonical_temp_root(dir.path());
    let store_dir = root.join("store");

    write_project(&root, "oldLeaf");
    let (initial, _) = CallGraphStore::cold_build_with_lease(
        store_dir.clone(),
        root.clone(),
        &project_files(&root),
    )
    .unwrap();
    assert_eq!(entry_leaf(&initial), "oldLeaf");
    drop(initial);

    let alternate_store_dir = root.join("alternate-store");
    write_project(&root, "freshLeaf");
    let (alternate, _) = CallGraphStore::cold_build_with_lease(
        alternate_store_dir.clone(),
        root.clone(),
        &project_files(&root),
    )
    .unwrap();
    assert_eq!(entry_leaf(&alternate), "freshLeaf");
    let alternate_generation = current_generation(&alternate_store_dir, &root);
    let alternate_sqlite = alternate_store_dir.join(&alternate_generation);
    drop(alternate);

    write_project(&root, "staleLeaf");
    let project_key = aft::search_index::artifact_cache_key(&root);
    let w2_generation = format!("{project_key}.g.adversarial-w2.sqlite");
    let w2_sqlite = store_dir.join(&w2_generation);
    let pointer = pointer_path(&store_dir, &root);
    let observer_ran = Arc::new(AtomicBool::new(false));
    let observer_ran_for_hook = Arc::clone(&observer_ran);
    let store_dir_for_hook = store_dir.clone();
    let alternate_sqlite_for_hook = alternate_sqlite.clone();
    let w2_sqlite_for_hook = w2_sqlite.clone();
    let w2_generation_for_hook = w2_generation.clone();
    let pointer_for_hook = pointer.clone();
    aft::callgraph_store::set_cold_build_swap_observer(Some(Arc::new(move |_tmp, _target| {
        copy_sqlite_file_set(&alternate_sqlite_for_hook, &w2_sqlite_for_hook);
        fs::write(&pointer_for_hook, format!("{w2_generation_for_hook}\n")).unwrap();

        let lease_path = store_dir_for_hook.join("writer.lease");
        let mut lease: serde_json::Value =
            serde_json::from_slice(&fs::read(&lease_path).unwrap()).unwrap();
        lease["writer_epoch"] = json!("reclaimed-by-adversarial-test");
        lease["pid"] = json!(std::process::id());
        fs::write(&lease_path, serde_json::to_vec(&lease).unwrap()).unwrap();
        observer_ran_for_hook.store(true, Ordering::SeqCst);
    })));
    let _observer_reset = ObserverReset;

    let error = match CallGraphStore::cold_build_with_lease(
        store_dir.clone(),
        root.clone(),
        &project_files(&root),
    ) {
        Ok(_) => panic!("stale writer publish unexpectedly succeeded"),
        Err(error) => error.to_string(),
    };
    assert!(observer_ran.load(Ordering::SeqCst));
    assert!(
        error.contains("lost epoch"),
        "stale writer publish must fail at the epoch fence, got: {error}"
    );

    assert_eq!(current_generation(&store_dir, &root), w2_generation);
    let winner = CallGraphStore::open_readonly(store_dir.clone(), root.clone())
        .unwrap()
        .expect("winner generation should remain published");
    assert_eq!(entry_leaf(&winner), "freshLeaf");
    assert_sqlite_integrity_ok(&w2_sqlite);
}

#[test]
fn root_keyed_reader_pin_survives_gc_until_marker_drops() {
    let dir = tempdir().unwrap();
    let root = canonical_temp_root(dir.path());
    let store_dir = root.join("store");

    let gen1 = publish_project_generation(&root, &store_dir, "firstLeaf");
    let gen1_path = store_dir.join(&gen1);
    let held_reader = CallGraphStore::open_readonly(store_dir.clone(), root.clone())
        .unwrap()
        .expect("first generation should open read-only");
    assert_eq!(entry_leaf(&held_reader), "firstLeaf");
    assert!(aft::root_cache::protected_read_marker_exists(
        &store_dir, &gen1
    ));

    let gen2 = publish_project_generation(&root, &store_dir, "secondLeaf");
    assert_ne!(gen1, gen2);
    let gen3 = publish_project_generation(&root, &store_dir, "thirdLeaf");
    assert_ne!(gen2, gen3);
    assert!(
        gen1_path.exists(),
        "a reader marker must keep an older-than-previous generation alive during GC"
    );
    assert_eq!(entry_leaf(&held_reader), "firstLeaf");

    drop(held_reader);
    assert!(!aft::root_cache::protected_read_marker_exists(
        &store_dir, &gen1
    ));
    let gen4 = publish_project_generation(&root, &store_dir, "fourthLeaf");
    assert_ne!(gen3, gen4);
    wait_until(Duration::from_secs(10), || !gen1_path.exists())
        .expect("the unpinned old generation should be reclaimed by the next GC sweep");
}

#[test]
fn root_keyed_migration_mid_crash_cleans_partial_and_preserves_legacy_source() {
    let dir = tempdir().unwrap();
    let root = canonical_temp_root(dir.path());
    let storage = root.join("storage");
    let legacy_dir = storage.join("opencode/callgraph");
    let legacy_build_dir = root.join("legacy-build");

    publish_project_generation(&root, &legacy_build_dir, "oldLeaf");
    publish_project_generation(&root, &legacy_build_dir, "newLeaf");
    copy_dir_all(&legacy_build_dir, &legacy_dir).unwrap();

    let legacy_source = newest_superseded_generation(&legacy_dir, &root)
        .expect("two legacy builds should leave a superseded source generation");
    let legacy_parts_before = sqlite_file_set_parts(&legacy_source);
    let legacy_hash_before = sqlite_file_set_hash(&legacy_source);

    // Migration now runs on a background maintenance lane in production, so
    // the crash/retry mechanics are driven synchronously here: the failure
    // seam is thread-local and must fire on the thread running the migration.
    let ctx_for_dirs = root_keyed_context(&root, &storage);
    let root_keyed_dir = ctx_for_dirs.callgraph_store_dir();

    aft::callgraph_store::set_legacy_migration_fail_after_temp_copy_for_test(true);
    let _fail_reset = MigrationFailReset;
    // A failing migration is swallowed into the legacy-fallback path by
    // design (queries must keep working); the direct entry point reports it
    // as Ok(None) because the fallback duplicate is filtered for callers that
    // already hold one.
    let failed = CallGraphStore::migrate_legacy_with_lease(root_keyed_dir.clone(), root.clone());
    assert!(
        matches!(failed, Ok(None)),
        "fail-after-temp-copy seam should abort the migration (got a published store instead): {failed:?}"
    );
    // While migration is failing, readers still get the legacy fallback.
    let fallback = CallGraphStore::open_readonly(root_keyed_dir.clone(), root.clone())
        .unwrap()
        .expect("failed migration should leave legacy data readable via fallback");
    assert!(fallback.sqlite_path().starts_with(&legacy_dir));
    drop(fallback);
    assert_eq!(
        sqlite_file_set_hash(&legacy_source),
        legacy_hash_before,
        "legacy source changed after failed migration: before parts={legacy_parts_before:?} after parts={:?}",
        sqlite_file_set_parts(&legacy_source)
    );

    let partials_after_failure = incomplete_migration_artifacts(&root_keyed_dir, &root);
    assert!(
        !partials_after_failure.is_empty(),
        "the fail-after-temp-copy seam should leave an incomplete migration artifact"
    );
    aft::callgraph_store::set_legacy_migration_fail_after_temp_copy_for_test(false);

    let migrated = CallGraphStore::migrate_legacy_with_lease(root_keyed_dir.clone(), root.clone())
        .unwrap()
        .expect("retry should clean the partial copy and migrate cleanly");
    assert!(migrated.sqlite_path().starts_with(&root_keyed_dir));
    assert_eq!(entry_leaf(&migrated), "oldLeaf");
    drop(migrated);

    assert_eq!(
        sqlite_file_set_hash(&legacy_source),
        legacy_hash_before,
        "legacy source changed after retry: after parts={:?}",
        sqlite_file_set_parts(&legacy_source)
    );
    assert!(
        incomplete_migration_artifacts(&root_keyed_dir, &root).is_empty(),
        "cleanup_incomplete_migrations should remove stale temp/unmanifested files before retry"
    );
    let generation = current_generation(&root_keyed_dir, &root);
    assert!(root_keyed_dir
        .join(format!("{generation}.migration.json"))
        .is_file());
}

#[test]
fn root_keyed_disk_floor_breach_uses_legacy_fallback_without_copy_or_cold_build() {
    let dir = tempdir().unwrap();
    let root = canonical_temp_root(dir.path());
    let storage = root.join("storage");
    let legacy_dir = storage.join("opencode/callgraph");
    let legacy_build_dir = root.join("legacy-build");

    publish_project_generation(&root, &legacy_build_dir, "floorLeaf");
    copy_dir_all(&legacy_build_dir, &legacy_dir).unwrap();
    let legacy_current = legacy_dir.join(current_generation(&legacy_dir, &root));
    let legacy_hash_before = sqlite_file_set_hash(&legacy_current);

    let cold_build_ran = Arc::new(AtomicBool::new(false));
    let cold_build_ran_for_hook = Arc::clone(&cold_build_ran);
    aft::callgraph_store::set_cold_build_swap_observer(Some(Arc::new(move |_tmp, _target| {
        cold_build_ran_for_hook.store(true, Ordering::SeqCst);
    })));
    let _observer_reset = ObserverReset;
    aft::callgraph_store::set_legacy_migration_available_disk_for_test(Some(0));
    let _disk_reset = DiskOverrideReset;

    let ctx = root_keyed_context(&root, &storage);
    let fallback = ctx
        .ensure_callgraph_store()
        .unwrap()
        .expect("disk-floor breach should still serve legacy fallback");
    assert!(fallback.sqlite_path().starts_with(&legacy_dir));
    assert_eq!(entry_leaf(&fallback), "floorLeaf");
    drop(fallback);

    assert!(!pointer_path(&ctx.callgraph_store_dir(), &root).exists());
    assert!(!cold_build_ran.load(Ordering::SeqCst));
    assert_eq!(
        sqlite_file_set_hash(&legacy_current),
        legacy_hash_before,
        "legacy current changed after disk-floor fallback: after parts={:?}",
        sqlite_file_set_parts(&legacy_current)
    );
}

#[test]
fn root_keyed_old_binary_legacy_writer_does_not_corrupt_or_delete_legacy_data() {
    let dir = tempdir().unwrap();
    let root = canonical_temp_root(dir.path());
    let storage = root.join("storage");
    let legacy_dir = storage.join("opencode/callgraph");
    let legacy_build_dir = root.join("legacy-build");

    publish_project_generation(&root, &legacy_build_dir, "migratedLeaf");
    publish_project_generation(&root, &legacy_build_dir, "legacyCurrentLeaf");
    copy_dir_all(&legacy_build_dir, &legacy_dir).unwrap();

    let ctx = root_keyed_context(&root, &storage);
    // First access serves the legacy fallback and schedules the migration on
    // the background lane; poll until the migrated root-keyed store installs.
    let first = ctx
        .ensure_callgraph_store()
        .unwrap()
        .expect("legacy fallback should be readable while migration runs");
    assert!(first.is_legacy_fallback());
    drop(first);
    wait_until(Duration::from_secs(20), || {
        // The background result installs via the main loop's drain in
        // production; drive it here like the runtime would.
        aft::runtime_drain::drain_callgraph_store_events(&ctx);
        matches!(
            ctx.ensure_callgraph_store(),
            Ok(Some(store)) if !store.is_legacy_fallback()
        )
    })
    .expect("background legacy migration should publish a root-keyed generation");
    let migrated = ctx
        .ensure_callgraph_store()
        .unwrap()
        .expect("root-keyed side should migrate from legacy");
    assert_eq!(entry_leaf(migrated.as_ref()), "migratedLeaf");
    let root_keyed_generation = current_generation(&ctx.callgraph_store_dir(), &root);
    let root_keyed_sqlite = ctx.callgraph_store_dir().join(&root_keyed_generation);
    drop(migrated);

    let old_binary_build_dir = root.join("old-binary-build");
    publish_project_generation(&root, &old_binary_build_dir, "legacyDriftLeaf");
    copy_dir_all(&old_binary_build_dir, &legacy_dir).unwrap();

    let legacy_reader = CallGraphStore::open_readonly(legacy_dir.clone(), root.clone())
        .unwrap()
        .expect("legacy partition should remain readable after an old-binary write");
    assert_eq!(entry_leaf(&legacy_reader), "legacyDriftLeaf");
    assert_sqlite_integrity_ok(legacy_reader.sqlite_path());
    drop(legacy_reader);

    let root_keyed_reader = CallGraphStore::open_readonly(ctx.callgraph_store_dir(), root.clone())
        .unwrap()
        .expect("root-keyed copy should remain readable after legacy drift");
    assert_eq!(entry_leaf(&root_keyed_reader), "migratedLeaf");
    assert_eq!(
        current_generation(&ctx.callgraph_store_dir(), &root),
        root_keyed_generation
    );
    assert!(root_keyed_sqlite.is_file());
    assert_sqlite_integrity_ok(root_keyed_reader.sqlite_path());
}

#[test]
fn root_keyed_daemon_restart_mid_write_keeps_pointer_valid_and_db_integrity_ok() {
    let dir = tempdir().unwrap();
    let root = canonical_temp_root(dir.path());
    let store_dir = root.join("store");

    let gen1 = publish_project_generation(&root, &store_dir, "oldLeaf");
    let pointer = pointer_path(&store_dir, &root);
    assert_eq!(current_generation(&store_dir, &root), gen1);
    write_project(&root, "newLeaf");

    let ready = root.join("abort-after-generation-rename.ready");
    let mut child =
        spawn_adversarial_helper("abort-after-generation-rename", &root, &store_dir, &ready);
    let status = wait_for_helper_exit(&mut child, &ready, Duration::from_secs(30));
    assert!(
        !status.success(),
        "helper should be terminated while the publish transaction is in flight"
    );

    let pointer_generation = fs::read_to_string(&pointer).unwrap();
    let pointer_generation = pointer_generation.trim();
    assert_eq!(pointer_generation, gen1);
    assert!(store_dir.join(pointer_generation).is_file());

    let reopened = CallGraphStore::open_ready_no_rebuild(store_dir.clone(), root.clone())
        .unwrap()
        .expect("restart should open the previous valid generation, not a dangling pointer");
    assert_eq!(entry_leaf(&reopened), "oldLeaf");
    assert_sqlite_integrity_ok(reopened.sqlite_path());
    drop(reopened);

    for generation in generation_files(&store_dir, &root) {
        assert_sqlite_integrity_ok(&store_dir.join(generation));
    }

    let (rebuilt, _) = CallGraphStore::cold_build_with_lease(
        store_dir.clone(),
        root.clone(),
        &project_files(&root),
    )
    .unwrap();
    assert_eq!(entry_leaf(&rebuilt), "newLeaf");
    let rebuilt_generation = current_generation(&store_dir, &root);
    assert!(store_dir.join(rebuilt_generation).is_file());
    assert_sqlite_integrity_ok(rebuilt.sqlite_path());
}

#[test]
#[ignore]
fn root_keyed_adversarial_child() {
    let Ok(mode) = std::env::var("ROOT_KEYED_ADVERSARIAL_MODE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("ROOT_KEYED_ADVERSARIAL_ROOT").unwrap());
    let store_dir = PathBuf::from(std::env::var_os("ROOT_KEYED_ADVERSARIAL_STORE_DIR").unwrap());
    let ready = PathBuf::from(std::env::var_os("ROOT_KEYED_ADVERSARIAL_READY").unwrap());

    match mode.as_str() {
        "hold-writer" => {
            let store = CallGraphStore::open(store_dir, root).unwrap();
            fs::write(
                &ready,
                store.writer_epoch_for_test().unwrap_or("missing epoch"),
            )
            .unwrap();
            loop {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        "abort-after-generation-rename" => {
            let ready_for_hook = ready.clone();
            aft::callgraph_store::set_cold_build_swap_observer(Some(Arc::new(
                move |_tmp, target| {
                    fs::write(&ready_for_hook, target.display().to_string()).unwrap();
                    std::process::abort();
                },
            )));
            let _ = CallGraphStore::cold_build_with_lease(
                store_dir,
                root.clone(),
                &project_files(&root),
            );
        }
        other => panic!("unknown ROOT_KEYED_ADVERSARIAL_MODE {other}"),
    }
}

struct ObserverReset;

impl Drop for ObserverReset {
    fn drop(&mut self) {
        aft::callgraph_store::set_cold_build_swap_observer(None);
    }
}

struct MigrationFailReset;

impl Drop for MigrationFailReset {
    fn drop(&mut self) {
        aft::callgraph_store::set_legacy_migration_fail_after_temp_copy_for_test(false);
    }
}

struct DiskOverrideReset;

impl Drop for DiskOverrideReset {
    fn drop(&mut self) {
        aft::callgraph_store::set_legacy_migration_available_disk_for_test(None);
    }
}

fn canonical_temp_root(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn write_project(root: &Path, leaf: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("main.ts"),
        format!("export function entry() {{ {leaf}(); }}\nfunction {leaf}() {{}}\n"),
    )
    .unwrap();
}

fn project_files(root: &Path) -> Vec<PathBuf> {
    walk_project_files(root).collect()
}

fn publish_project_generation(root: &Path, store_dir: &Path, leaf: &str) -> String {
    write_project(root, leaf);
    let (store, _) = CallGraphStore::cold_build_with_lease(
        store_dir.to_path_buf(),
        root.to_path_buf(),
        &project_files(root),
    )
    .unwrap();
    assert_eq!(entry_leaf(&store), leaf);
    let generation = current_generation(store_dir, root);
    drop(store);
    generation
}

fn entry_leaf(store: &impl CallGraphRead) -> String {
    store
        .call_tree(Path::new("main.ts"), "entry", 1)
        .unwrap()
        .children[0]
        .name
        .clone()
}

fn root_keyed_context(root: &Path, storage: &Path) -> AppContext {
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            indexes: aft::config::IndexesConfig {
                trigram: false,
                semantic: false,
                callgraph: true,
            },
            ..Config::default()
        },
    );
    ctx.set_harness(Harness::Opencode);
    ctx.set_canonical_cache_root(root.to_path_buf());
    ctx.set_cache_role(false, None);
    // The lease gate is fail-closed for unregistered roots; grant writer
    // capability the way a real configure does so migrations and cold builds
    // reach the failure seams these tests exercise.
    aft::root_cache::configure_artifact_access(
        root,
        &aft::search_index::artifact_cache_key(root),
        false,
    );
    ctx
}

fn pointer_path(store_dir: &Path, root: &Path) -> PathBuf {
    let key = aft::search_index::artifact_cache_key(root);
    store_dir.join(format!("{key}.current"))
}

fn current_generation(store_dir: &Path, root: &Path) -> String {
    fs::read_to_string(pointer_path(store_dir, root))
        .unwrap()
        .trim()
        .to_string()
}

fn generation_files(store_dir: &Path, root: &Path) -> Vec<String> {
    let prefix = format!("{}.g", aft::search_index::artifact_cache_key(root));
    let mut files = fs::read_dir(store_dir)
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&prefix) && name.ends_with(".sqlite"))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn newest_superseded_generation(store_dir: &Path, root: &Path) -> Option<PathBuf> {
    let current = current_generation(store_dir, root);
    let prefix = format!("{}.g", aft::search_index::artifact_cache_key(root));
    let mut candidates = fs::read_dir(store_dir)
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name == current
                || name.contains(".tmp.")
                || !name.starts_with(&prefix)
                || !name.ends_with(".sqlite")
            {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    candidates.into_iter().next().map(|(_, path)| path)
}

fn incomplete_migration_artifacts(store_dir: &Path, root: &Path) -> Vec<String> {
    let prefix = format!("{}.g", aft::search_index::artifact_cache_key(root));
    let mut artifacts = fs::read_dir(store_dir)
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            if !name.starts_with(&prefix) || !name.contains(".migrated.") {
                return false;
            }
            name.contains(".tmp.")
                || (name.ends_with(".sqlite")
                    && !store_dir.join(format!("{name}.migration.json")).is_file())
        })
        .collect::<Vec<_>>();
    artifacts.sort();
    artifacts
}

fn sqlite_member(path: &Path, suffix: &str) -> PathBuf {
    if suffix.is_empty() {
        path.to_path_buf()
    } else {
        PathBuf::from(format!("{}{suffix}", path.display()))
    }
}

fn copy_sqlite_file_set(source: &Path, destination: &Path) {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    for suffix in SQLITE_FILE_SET_SUFFIXES {
        let source_path = sqlite_member(source, suffix);
        if !source_path.is_file() {
            continue;
        }
        fs::copy(source_path, sqlite_member(destination, suffix)).unwrap();
    }
}

fn sqlite_file_set_hash(path: &Path) -> String {
    let mut hasher = blake3::Hasher::new();
    for (suffix, bytes, hash) in sqlite_file_set_parts(path) {
        hasher.update(suffix.as_bytes());
        hasher.update(bytes.to_string().as_bytes());
        hasher.update(hash.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn sqlite_file_set_parts(path: &Path) -> Vec<(String, u64, String)> {
    let mut parts = Vec::new();
    for suffix in SQLITE_FILE_SET_SUFFIXES {
        let member = sqlite_member(path, suffix);
        if !member.is_file() {
            continue;
        }
        // SQLite may refresh the shared-memory sidecar when a read-only
        // connection attaches; the source data files must stay byte-stable.
        if *suffix == "-shm" {
            continue;
        }
        let bytes = fs::read(member).unwrap();
        // SQLite can materialize empty rollback/WAL sidecars when attaching a
        // read-only connection; only sidecars containing pages are source data.
        if bytes.is_empty() && (*suffix == "-wal" || *suffix == "-journal") {
            continue;
        }
        parts.push((
            suffix.to_string(),
            bytes.len() as u64,
            blake3::hash(&bytes).to_hex().to_string(),
        ));
    }
    parts
}

fn assert_sqlite_integrity_ok(path: &Path) {
    let conn = Connection::open(path).unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        integrity,
        "ok",
        "integrity_check failed for {}",
        path.display()
    );
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn spawn_adversarial_helper(mode: &str, root: &Path, store_dir: &Path, ready: &Path) -> Child {
    let exe = std::env::current_exe().unwrap();
    Command::new(exe)
        .arg("--exact")
        .arg(HELPER_TEST_NAME)
        .arg("--ignored")
        .arg("--nocapture")
        .env("ROOT_KEYED_ADVERSARIAL_MODE", mode)
        .env("ROOT_KEYED_ADVERSARIAL_ROOT", root.as_os_str())
        .env("ROOT_KEYED_ADVERSARIAL_STORE_DIR", store_dir.as_os_str())
        .env("ROOT_KEYED_ADVERSARIAL_READY", ready.as_os_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for_helper_ready(child: &mut Child, ready: &Path, budget: Duration) {
    let started = Instant::now();
    loop {
        if ready.is_file() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let output = child_output(child);
            panic!(
                "helper exited before ready file {} appeared: status={status}, output={output}",
                ready.display()
            );
        }
        assert!(
            started.elapsed() < budget,
            "timed out waiting for helper ready file {}",
            ready.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_helper_exit(child: &mut Child, ready: &Path, budget: Duration) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                ready.is_file(),
                "helper exited before publish hook wrote ready file {}; output={}",
                ready.display(),
                child_output(child)
            );
            return status;
        }
        if started.elapsed() >= budget {
            let _ = child.kill();
            let status = child.wait().unwrap();
            panic!(
                "timed out waiting for helper exit; killed child with status {status}; output={}",
                child_output(child)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn child_output(child: &mut Child) -> String {
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        if !text.is_empty() {
            output.push_str("stdout:\n");
            output.push_str(&text);
        }
    }
    if let Some(mut stderr) = child.stderr.take() {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        if !text.is_empty() {
            output.push_str("stderr:\n");
            output.push_str(&text);
        }
    }
    output
}

fn wait_until(budget: Duration, mut condition: impl FnMut() -> bool) -> Result<(), ()> {
    let started = Instant::now();
    while started.elapsed() < budget {
        if condition() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    condition().then_some(()).ok_or(())
}

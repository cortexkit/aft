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

    // The fallback is returned before migration finishes. Observe the worker's
    // terminal event before checking that it did not publish any artifacts.
    let receiver = ctx
        .callgraph_store_rx()
        .lock()
        .as_ref()
        .expect("legacy fallback should schedule background migration")
        .clone();
    let completion = receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("background legacy migration should complete");

    assert!(!pointer_path(&ctx.callgraph_store_dir(), &root).exists());
    assert!(matches!(
        completion,
        aft::context::CallGraphStoreBuildEvent::Settled
    ));
    assert!(generation_files(&ctx.callgraph_store_dir(), &root).is_empty());
    assert!(incomplete_migration_artifacts(&ctx.callgraph_store_dir(), &root).is_empty());
    assert!(!cold_build_ran.load(Ordering::SeqCst));
    assert_eq!(
        sqlite_file_set_hash(&legacy_current),
        legacy_hash_before,
        "legacy current changed after disk-floor fallback: after parts={:?}",
        sqlite_file_set_parts(&legacy_current)
    );
}

#[test]
fn root_keyed_background_cold_build_inherits_swap_observer() {
    let dir = tempdir().unwrap();
    let root = canonical_temp_root(dir.path());
    write_project(&root, "backgroundLeaf");
    let ctx = root_keyed_context(&root, &root.join("storage"));

    let cold_build_ran = Arc::new(AtomicBool::new(false));
    let cold_build_ran_for_hook = Arc::clone(&cold_build_ran);
    aft::callgraph_store::set_cold_build_swap_observer(Some(Arc::new(move |_tmp, _target| {
        cold_build_ran_for_hook.store(true, Ordering::SeqCst);
    })));
    let _observer_reset = ObserverReset;

    assert!(matches!(
        ctx.callgraph_store_for_ops(),
        aft::context::CallgraphStoreAccess::Building
    ));
    let receiver = ctx
        .callgraph_store_rx()
        .lock()
        .as_ref()
        .expect("cold build should install a background receiver")
        .clone();
    let completion = receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("background cold build should complete");
    assert!(matches!(
        completion,
        aft::context::CallGraphStoreBuildEvent::Ready { .. }
    ));
    assert!(pointer_path(&ctx.callgraph_store_dir(), &root).exists());
    assert!(cold_build_ran.load(Ordering::SeqCst));
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
        crate::context_storage::isolate(Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            indexes: aft::config::IndexesConfig {
                trigram: false,
                semantic: false,
                callgraph: true,
            },
            ..Config::default()
        }),
    )
    .with_test_checkpoint_store();
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

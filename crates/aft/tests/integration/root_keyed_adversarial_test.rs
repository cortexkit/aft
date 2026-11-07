use aft::callgraph::walk_project_files;
use aft::callgraph_store::{CallGraphRead, CallGraphStore};
use rusqlite::Connection;
use serde_json::json;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tempfile::tempdir;

const HELPER_TEST_NAME: &str = "root_keyed_adversarial_test::root_keyed_adversarial_child";
const SQLITE_FILE_SET_SUFFIXES: &[&str] = &["", "-wal", "-shm", "-journal"];

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


struct MigrationFailReset;

impl Drop for MigrationFailReset {
    fn drop(&mut self) {
        aft::callgraph_store::set_legacy_migration_fail_after_temp_copy_for_test(false);
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

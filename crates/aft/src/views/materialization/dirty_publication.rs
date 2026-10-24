//! Full-publication accounting using the existing commit and checkpoint hooks.

use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct Database {
    hooks: Vec<i32>,
    checkpoints: Vec<(i64, i64, i64)>,
    wal_bytes: u64,
    main_bytes: u64,
    pages: Vec<u32>,
    owners: BTreeMap<u32, String>,
}

struct Publication {
    root: PathBuf,
    databases: BTreeMap<PathBuf, Database>,
}

static PUBLICATION: Mutex<Option<Publication>> = Mutex::new(None);

fn database_path(db: *mut ffi::sqlite3) -> PathBuf {
    let name = unsafe { ffi::sqlite3_db_filename(db, c"main".as_ptr()) };
    if name.is_null() {
        return PathBuf::new();
    }
    PathBuf::from(unsafe { CStr::from_ptr(name) }.to_string_lossy().as_ref())
}

pub(crate) unsafe fn probe_wal(db: *mut ffi::sqlite3, frames: i32) {
    let mut publication = PUBLICATION.lock().unwrap();
    let Some(publication) = publication.as_mut() else {
        return;
    };
    let path = database_path(db);
    if path.starts_with(&publication.root) {
        publication
            .databases
            .entry(path)
            .or_default()
            .hooks
            .push(frames);
    }
}

pub(crate) unsafe fn probe_checkpoint(db: *mut ffi::sqlite3, busy: i64, log: i64, backfilled: i64) {
    let mut publication = PUBLICATION.lock().unwrap();
    let Some(publication) = publication.as_mut() else {
        return;
    };
    let path = database_path(db);
    if !path.starts_with(&publication.root) {
        return;
    }
    let database = publication.databases.entry(path).or_default();
    database.checkpoints.push((busy, log, backfilled));
    // from_handle borrows the existing SQLite handle; it neither opens another
    // database connection nor closes the handle when this wrapper is dropped.
    let borrowed = unsafe { Connection::from_handle(db) }.unwrap();
    database.owners.extend(owners(&borrowed));
}

unsafe extern "C" fn hook(
    _: *mut c_void,
    db: *mut ffi::sqlite3,
    _: *const c_char,
    frames: c_int,
) -> c_int {
    unsafe { probe_wal(db, frames) };
    ffi::SQLITE_OK
}

pub(crate) fn observe(connection: &Connection, path: &Path) {
    let mut publication = PUBLICATION.lock().unwrap();
    let Some(publication) = publication.as_mut() else {
        return;
    };
    let path = path.canonicalize().unwrap();
    if !path.starts_with(&publication.root) {
        return;
    }
    publication
        .databases
        .entry(path)
        .or_default()
        .owners
        .extend(owners(connection));
    unsafe {
        ffi::sqlite3_wal_hook(connection.handle(), Some(hook), std::ptr::null_mut());
    }
}

pub(crate) fn probe_connection(connection: &Connection) {
    if PUBLICATION.lock().unwrap().is_none() {
        return;
    }
    observe(connection, &database_path(unsafe { connection.handle() }));
}

pub(crate) fn finish(connection: &Connection) {
    let mut publication = PUBLICATION.lock().unwrap();
    let Some(publication) = publication.as_mut() else {
        return;
    };
    let path = database_path(unsafe { connection.handle() });
    if path.starts_with(&publication.root) {
        publication
            .databases
            .entry(path)
            .or_default()
            .owners
            .extend(owners(connection));
    }
}

pub(super) fn written(path: &Path, wal: bool, buffer: *const c_void, amount: c_int, offset: i64) {
    let mut publication = PUBLICATION.lock().unwrap();
    let Some(publication) = publication.as_mut() else {
        return;
    };
    let database_path = if wal {
        PathBuf::from(path.to_str().unwrap().strip_suffix("-wal").unwrap())
    } else {
        path.to_path_buf()
    };
    if !database_path.starts_with(&publication.root) {
        return;
    }
    let database = publication.databases.entry(database_path).or_default();
    if wal {
        database.wal_bytes += amount as u64;
        if offset >= 32 && (offset - 32) % 4120 == 0 && amount >= 24 {
            let bytes = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), 4) };
            database
                .pages
                .push(u32::from_be_bytes(bytes.try_into().unwrap()));
        }
    } else {
        database.main_bytes += amount as u64;
    }
}

struct PublicationGuard;

impl Drop for PublicationGuard {
    fn drop(&mut self) {
        *PUBLICATION.lock().unwrap() = None;
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .env("TMPDIR", std::env::temp_dir().canonicalize().unwrap())
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "fresh full own-seed publications; run single-threaded with isolated HOME/XDG"]
fn derived_dirty_publication_probe() {
    use crate::alias::head_tree_entries;
    use crate::views::assembly::{head_tree_fingerprint, prepare_checkout, AssemblyRequest};
    use crate::views::ViewStore;
    for size in [1000, 5000, 20000] {
        for case in ["function", "file", "noop", "300-path"] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap().join("checkout");
            std::fs::create_dir(&root).unwrap();
            for i in 0..size {
                let mut source = format!("export function value_{i}() {{ return 1; }} export function caller_{i}() {{ return value_{i}(); }}");
                if i == 0 {
                    for j in 0..512 {
                        source.push_str(&format!(
                            "\nexport function extra_{j}() {{ return value_0(); }}"
                        ));
                    }
                }
                std::fs::write(root.join(format!("file_{i:05}.ts")), source).unwrap();
            }
            git(&root, &["init", "--quiet"]);
            git(&root, &["add", "."]);
            git(
                &root,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "fixture",
                ],
            );
            let storage = directory.path().canonicalize().unwrap().join("store");
            std::fs::create_dir(&storage).unwrap();
            let mut request = AssemblyRequest {
                storage: storage.clone(),
                project_root: root.clone(),
                family: crate::search_index::artifact_cache_key(&root),
                scope: crate::path_identity::project_scope_key(&root),
                desired_head: head_tree_fingerprint(&head_tree_entries(&root).unwrap()),
                changed_paths: Default::default(),
                semantic_keys: Default::default(),
                require_semantic: false,
                allow_blob_put: true,
                callgraph: true,
            };
            prepare_checkout(&request, &mut |_| Ok(()))
                .unwrap()
                .commit()
                .unwrap();
            let view = ViewStore::open(&storage, &request.scope).unwrap();
            crate::views::generation::wait_for_derived_checkpoint_for_test(view.view_dir());
            for i in 0..if case == "300-path" {
                300
            } else if case == "noop" {
                0
            } else {
                1
            } {
                let path = format!("file_{i:05}.ts");
                let source = std::fs::read_to_string(root.join(&path)).unwrap();
                let next = if case == "function" {
                    source.replacen("return value_0()", "return missing_0()", 1)
                } else {
                    format!("\n{source}")
                };
                std::fs::write(root.join(&path), next).unwrap();
                request.changed_paths.insert(path.into_bytes());
            }
            let session = Session::install();
            *PUBLICATION.lock().unwrap() = Some(Publication {
                root: storage.clone(),
                databases: Default::default(),
            });
            let guard = PublicationGuard;
            let mut prepared = prepare_checkout(&request, &mut |_| Ok(())).unwrap();
            let (seed, mechanism, reason) = crate::views::assembly::seed_labels_for_test(&prepared);
            let report = prepared.commit().unwrap();
            crate::views::generation::wait_for_derived_checkpoint_for_test(view.view_dir());
            let publication = PUBLICATION.lock().unwrap().take().unwrap();
            drop(guard);
            drop(session);
            eprintln!("PUBLICATION_CASE size={size} case={case} seed={seed} mechanism={mechanism} cold_reason={reason}");
            if matches!(case, "function" | "file") {
                assert_eq!(seed, "own");
            }
            for (path, database) in publication.databases {
                let mut by_owner = BTreeMap::<String, usize>::new();
                for page in database.pages {
                    *by_owner
                        .entry(
                            database
                                .owners
                                .get(&page)
                                .cloned()
                                .unwrap_or_else(|| "<unmapped>".into()),
                        )
                        .or_default() += 1;
                }
                eprintln!(
                    "PUBLICATION_DB {}",
                    serde_json::json!({
                        "path": path.strip_prefix(&storage).unwrap(), "wal_hook_frames": database.hooks,
                        "checkpoints": database.checkpoints, "wal_bytes": database.wal_bytes,
                        "main_bytes": database.main_bytes, "frame_header_writes_by_owner": by_owner,
                    })
                );
            }
            if let Some(generation) = report.generation.as_deref() {
                let derived = view.derived_path(generation).unwrap();
                assert_eq!(crate::db::file_identity::open_connections(&derived), 0);
                let blobs = crate::blob_store::BlobStore::open(
                    &storage,
                    &request.family,
                    crate::blob_store::BlobPlane::Callgraph,
                )
                .unwrap();
                let cold = directory.path().join("cold.sqlite");
                materialize_manifest_view_database(
                    &cold,
                    blobs.path(),
                    report.manifest.as_ref().unwrap(),
                )
                .unwrap();
                assert_eq!(snapshot(&derived), snapshot(&cold), "{size} {case}");
            }
        }
    }
}

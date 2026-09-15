use aft::callgraph_store::{project_dead_code_snapshot, CallGraphStore};
use rusqlite::{params, Connection};
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
#[ignore = "requires AFT_STALE_REPRO_STORE and AFT_STALE_REPRO_ROOT"]
fn stale_deleted_store_copy_reproduction() {
    let store_dir = PathBuf::from(
        std::env::var_os("AFT_STALE_REPRO_STORE").expect("AFT_STALE_REPRO_STORE is set"),
    );
    let project_root = PathBuf::from(
        std::env::var_os("AFT_STALE_REPRO_ROOT").expect("AFT_STALE_REPRO_ROOT is set"),
    );
    let store = CallGraphStore::open_ready_repairing(store_dir, project_root)
        .expect("open copied store")
        .expect("copied store is ready");

    println!(
        "projection before empty refresh: {:?}",
        project_dead_code_snapshot(store.sqlite_path()).map(|snapshot| snapshot.files.len())
    );
    let stats = store.refresh_files(&[]).expect("refresh copied store");
    println!("empty refresh stats: {stats:?}");
    println!(
        "projection after empty refresh: {:?}",
        project_dead_code_snapshot(store.sqlite_path()).map(|snapshot| snapshot.files.len())
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GraphRowCounts {
    nodes: i64,
    edges: i64,
    refs: i64,
    dispatch_hints: i64,
}

impl GraphRowCounts {
    fn totals(conn: &Connection) -> Self {
        Self {
            nodes: count(conn, "nodes", None),
            edges: count(conn, "edges", None),
            refs: count(conn, "refs", None),
            dispatch_hints: count(conn, "dispatch_hints", None),
        }
    }

    fn for_file(conn: &Connection, file: &str) -> Self {
        Self {
            nodes: count(conn, "nodes", Some(("file_path", file))),
            edges: conn
                .query_row(
                    "SELECT COUNT(*) FROM edges WHERE ref_id IN (SELECT ref_id FROM refs WHERE caller_file = ?1)",
                    params![file],
                    |row| row.get(0),
                )
                .unwrap(),
            refs: count(conn, "refs", Some(("caller_file", file))),
            dispatch_hints: count(conn, "dispatch_hints", Some(("file", file))),
        }
    }

    fn minus(self, removed: Self) -> Self {
        Self {
            nodes: self.nodes - removed.nodes,
            edges: self.edges - removed.edges,
            refs: self.refs - removed.refs,
            dispatch_hints: self.dispatch_hints - removed.dispatch_hints,
        }
    }
}

#[test]
fn stale_deleted_file_unmentioned_by_refresh_is_removed_and_projection_recovers() {
    let fixture = fixture_store();
    let doomed = fixture.root.join("src/doomed.ts");
    fixture.store.mark_files_stale(&[doomed.clone()]).unwrap();
    fs::remove_file(&doomed).unwrap();

    let before_projection = project_dead_code_snapshot(fixture.store.sqlite_path());
    assert!(
        before_projection.is_err(),
        "stale backend row must block projection before refresh"
    );
    let conn = Connection::open(fixture.store.sqlite_path()).unwrap();
    let totals_before = GraphRowCounts::totals(&conn);
    let doomed_before = GraphRowCounts::for_file(&conn, "src/doomed.ts");
    assert!(doomed_before.nodes > 0, "fixture must contain doomed nodes");
    assert!(doomed_before.edges > 0, "fixture must contain doomed edges");
    assert!(doomed_before.refs > 0, "fixture must contain doomed refs");
    assert!(
        doomed_before.dispatch_hints > 0,
        "fixture must contain doomed dispatch hints"
    );
    drop(conn);

    let stats = fixture
        .store
        .refresh_files(&[fixture.root.join("src/unrelated.ts")])
        .unwrap();

    assert_eq!(stats.deleted_files, vec!["src/doomed.ts"]);
    assert!(fixture.store.stale_files().unwrap().is_empty());
    project_dead_code_snapshot(fixture.store.sqlite_path()).expect("projection recovers");
    let conn = Connection::open(fixture.store.sqlite_path()).unwrap();
    assert_eq!(
        GraphRowCounts::for_file(&conn, "src/doomed.ts"),
        GraphRowCounts {
            nodes: 0,
            edges: 0,
            refs: 0,
            dispatch_hints: 0,
        }
    );
    assert_eq!(
        GraphRowCounts::totals(&conn),
        totals_before.minus(doomed_before),
        "refresh must remove only the deleted file's graph rows"
    );
}

#[test]
fn stale_existing_file_unmentioned_by_refresh_is_untouched() {
    let fixture = fixture_store();
    let doomed = fixture.root.join("src/doomed.ts");
    fixture.store.mark_files_stale(&[doomed]).unwrap();
    let conn = Connection::open(fixture.store.sqlite_path()).unwrap();
    let before = GraphRowCounts::totals(&conn);
    drop(conn);

    let stats = fixture.store.refresh_files(&[]).unwrap();

    assert!(stats.deleted_files.is_empty());
    assert_eq!(fixture.store.stale_files().unwrap(), vec!["src/doomed.ts"]);
    let conn = Connection::open(fixture.store.sqlite_path()).unwrap();
    assert_eq!(GraphRowCounts::totals(&conn), before);
    assert!(project_dead_code_snapshot(fixture.store.sqlite_path()).is_err());
}

#[cfg(unix)]
#[test]
fn stale_file_with_permission_denied_stat_is_untouched() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture_store();
    let guarded = fixture.root.join("guarded");
    let file = guarded.join("unreadable.ts");
    fs::create_dir_all(&guarded).unwrap();
    fs::write(&file, "export function unreadable() {}\n").unwrap();
    fixture.store.refresh_files(&[file.clone()]).unwrap();
    fixture.store.mark_files_stale(&[file]).unwrap();
    let original_mode = fs::metadata(&guarded).unwrap().permissions().mode();
    fs::set_permissions(&guarded, fs::Permissions::from_mode(0)).unwrap();

    let result = fixture.store.refresh_files(&[]);

    fs::set_permissions(&guarded, fs::Permissions::from_mode(original_mode)).unwrap();
    result.unwrap();
    assert_eq!(
        fixture.store.stale_files().unwrap(),
        vec!["guarded/unreadable.ts"]
    );
    assert_eq!(
        fixture
            .store
            .backend_status_for_file(&guarded.join("unreadable.ts"))
            .unwrap()
            .as_deref(),
        Some("stale")
    );
}

struct FixtureStore {
    _temp: tempfile::TempDir,
    root: PathBuf,
    store: CallGraphStore,
}

fn fixture_store() -> FixtureStore {
    let temp = tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/keep.ts"),
        "export function keep() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/unrelated.ts"),
        "export function unrelated() { return 2; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src/doomed.ts"),
        r#"import { keep } from "./keep";
const service = { execute() { return keep(); } };
export function doomed() {
  keep();
  return service.execute();
}
"#,
    )
    .unwrap();
    let root = fs::canonicalize(root).unwrap();
    let store = CallGraphStore::open(temp.path().join("store"), root.clone()).unwrap();
    store
        .cold_build(&[
            root.join("src/keep.ts"),
            root.join("src/unrelated.ts"),
            root.join("src/doomed.ts"),
        ])
        .unwrap();
    FixtureStore {
        _temp: temp,
        root,
        store,
    }
}

fn count(conn: &Connection, table: &str, filter: Option<(&str, &str)>) -> i64 {
    match filter {
        Some((column, value)) => conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
                params![value],
                |row| row.get(0),
            )
            .unwrap(),
        None => conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
            .unwrap(),
    }
}

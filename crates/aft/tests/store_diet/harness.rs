//! Offline storage parity oracle. Recording is explicit; ordinary tests only compare.
use super::*;
use crate::commands::callgraph_store_adapter as adapter;
use crate::parser::SymbolCache;
use serde_json::{json, Value};
use std::sync::RwLock;

const ASSETS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/store_diet");

fn golden(name: &str, bytes: &[u8]) {
    let path = Path::new(ASSETS).join(name);
    if std::env::var_os("AFT_STORE_DIET_RECORD").as_deref() == Some(std::ffi::OsStr::new("1")) {
        fs::write(path, bytes).unwrap();
    } else {
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "{} differs",
            path.display()
        );
    }
}

fn fixture_files(root: &Path) -> Vec<PathBuf> {
    let mut files = walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|s| s.to_str()),
                Some(
                    "ts" | "tsx"
                        | "js"
                        | "jsx"
                        | "rs"
                        | "py"
                        | "go"
                        | "java"
                        | "cs"
                        | "php"
                        | "rb"
                        | "c"
                        | "cpp"
                        | "swift"
                        | "kt"
                )
            )
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn replies(store: &CallGraphStore, root: &Path) -> Value {
    let file = root.join("main.ts");
    let target = root.join("helpers.ts");
    let cache = Arc::new(RwLock::new(SymbolCache::new()));
    json!({
        "callers": adapter::callers_result(store, &target, "validate", 4, true).unwrap(),
        "call_tree": adapter::call_tree_result(store, &file, "main", 4, true).unwrap(),
        "impact": adapter::impact_result(store, &target, "validate", 4, true).unwrap(),
        "trace_to": adapter::trace_to_result(store, &target, "validate", 4, true).unwrap(),
        "trace_to_symbol": adapter::trace_to_symbol_result(store, &file, "main", "validate", Some(&target), 4, true).unwrap(),
        "trace_data": adapter::trace_data_result(store, &root.join("data_flow.ts"), "transformData", "rawInput", 4, cache).unwrap(),
    })
}

fn counts(store: &CallGraphStore) -> Value {
    let conn = store.conn.lock().unwrap();
    let tables: Vec<Value> =
        serde_json::from_slice(&fs::read(Path::new(ASSETS).join("table_encoding.json")).unwrap())
            .unwrap();
    let mut result = serde_json::Map::new();
    for table in tables.iter().filter(|table| table["graph_data"] == true) {
        let name = table["table"].as_str().unwrap();
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [name],
                |row| row.get(0),
            )
            .unwrap();
        if present {
            let count: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {name}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            result.insert(name.to_owned(), json!(count));
        } else {
            result.insert(name.to_owned(), Value::Null);
        }
    }
    let unresolved: i64 = conn
        .query_row(
            "SELECT count(*) FROM refs WHERE status='unresolved'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    result.insert("refs_unresolved".into(), json!(unresolved));
    Value::Object(result)
}

fn fixture() -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
    let temp = tempfile::tempdir().unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/callgraph")
        .canonicalize()
        .unwrap();
    let files = fixture_files(&root);
    assert!(!files.is_empty());
    (temp, root, files)
}

#[test]
fn store_diet_legacy_golden() {
    let (temp, root, files) = fixture();
    let store = CallGraphStore::open(temp.path().join("legacy"), root.clone()).unwrap();
    store.cold_build(&files).unwrap();
    golden(
        "legacy-replies.json",
        &serde_json::to_vec_pretty(&replies(&store, &root)).unwrap(),
    );
    golden(
        "legacy-counts.json",
        &serde_json::to_vec_pretty(&counts(&store)).unwrap(),
    );
}

fn view_store(temp: &Path, root: &Path, files: &[PathBuf]) -> CallGraphStore {
    use crate::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};
    let blobs = temp.join("blobs.sqlite");
    let connection = Connection::open(&blobs).unwrap();
    connection.execute_batch("CREATE TABLE blob_payloads(full_key BLOB PRIMARY KEY, payload BLOB NOT NULL, payload_digest BLOB NOT NULL, payload_schema INTEGER NOT NULL)").unwrap();
    let mut entries = Vec::new();
    for file in files {
        let language = match file.extension().and_then(|s| s.to_str()).unwrap() {
            "rs" => "rust",
            "py" => "python",
            "go" => "go",
            "java" => "java",
            "cs" => "csharp",
            "js" => "javascript",
            _ => "typescript",
        };
        let source = fs::read_to_string(file).unwrap();
        let payload =
            join::CallgraphBlob::extract(&source, language, crate::views::callgraph::PRODUCER)
                .unwrap()
                .to_bytes()
                .unwrap();
        let key = blake3::hash(&payload);
        connection
            .execute(
                "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2, ?3, 1)",
                params![
                    key.as_bytes().as_slice(),
                    payload,
                    key.as_bytes().as_slice()
                ],
            )
            .unwrap();
        entries.push((
            RelPath::new(relative_path(root, file).as_bytes()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    callgraph: Some(key.to_hex().to_string()),
                    semantic: None,
                },
                resolution_input: false,
            },
        ));
    }
    drop(connection);
    let manifest = Manifest::new(entries).unwrap();
    let database = temp.join("derived.sqlite");
    crate::views::materialization::materialize_manifest_view_database(&database, &blobs, &manifest)
        .unwrap();
    let conn = open_readonly_connection(&database).unwrap();
    let mut store = CallGraphStore::from_connection(
        root.to_path_buf(),
        "store-diet".into(),
        database,
        temp.to_path_buf(),
        false,
        None,
        None,
        None,
        conn,
    );
    store.manifest_view = true;
    store.database_ready.store(true, AtomicOrdering::Release);
    store
}

#[test]
fn store_diet_views_golden() {
    let (temp, root, files) = fixture();
    let store = view_store(temp.path(), &root, &files);
    golden(
        "views-replies.json",
        &serde_json::to_vec_pretty(&replies(&store, &root)).unwrap(),
    );
    golden(
        "views-counts.json",
        &serde_json::to_vec_pretty(&counts(&store)).unwrap(),
    );
}

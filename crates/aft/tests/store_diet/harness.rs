//! Offline storage parity oracle. Recording is explicit; ordinary tests only compare.
use super::*;
use crate::commands::callgraph_store_adapter as adapter;
use crate::parser::SymbolCache;
use serde_json::{json, Value};
use std::fs;
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
    fn walk(root: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), files);
            } else if entry.file_type().unwrap().is_file() {
                files.push(entry.path());
            }
        }
    }
    let mut paths = Vec::new();
    walk(root, &mut paths);
    let mut files = paths
        .into_iter()
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
    golden(
        "legacy-seeded.json",
        &serde_json::to_vec_pretty(&seeded_replies(&store, &root)).unwrap(),
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
    golden(
        "views-seeded.json",
        &serde_json::to_vec_pretty(&seeded_replies(&store, &root)).unwrap(),
    );
}

fn plans(store: &CallGraphStore) -> Value {
    let rows: Vec<Value> =
        serde_json::from_slice(&fs::read(Path::new(ASSETS).join("statements.json")).unwrap())
            .unwrap();
    let conn = store.conn.lock().unwrap();
    // Other databases and transaction-local tables do not live in a finished
    // graph fixture. Keep their schemas on a separate in-memory connection.
    // Actual graph plans always use the real fixture's schema first.
    let auxiliary = Connection::open_in_memory().unwrap();
    let mut schema = conn.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND type IN ('table','index') ORDER BY type DESC").unwrap();
    for sql in schema.query_map([], |row| row.get::<_, String>(0)).unwrap() {
        let _ = auxiliary.execute_batch(&sql.unwrap());
    }
    for _ in 0..2 {
        for row in &rows {
            for variant in row["variants"].as_array().unwrap() {
                let sql = variant["sql"].as_str().unwrap();
                let upper = sql.trim_start().to_ascii_uppercase();
                if upper.starts_with("CREATE TABLE")
                    || upper.starts_with("CREATE TEMP TABLE")
                    || upper.starts_with("CREATE INDEX")
                    || upper.starts_with("CREATE UNIQUE INDEX")
                    || (upper.starts_with("ALTER TABLE") && upper.contains(" ADD COLUMN "))
                {
                    let _ = auxiliary.execute_batch(sql);
                }
            }
        }
    }
    let mut result = serde_json::Map::new();
    for row in rows {
        for variant in row["variants"].as_array().unwrap() {
            let key = format!(
                "{}:{}",
                row["id"].as_str().unwrap(),
                variant["variant"].as_str().unwrap()
            );
            let sql = variant["sql"].as_str().unwrap();
            let sql = sql.strip_prefix("EXPLAIN QUERY PLAN ").unwrap_or(sql);
            let explain = |connection: &Connection| -> rusqlite::Result<Vec<String>> {
                let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                let count = statement.parameter_count();
                let rows = statement.query_map(
                    params_from_iter(vec![rusqlite::types::Value::Null; count]),
                    |row| row.get::<_, String>(3),
                )?;
                rows.collect()
            };
            let plan = explain(&conn).or_else(|_| explain(&auxiliary));
            result.insert(
                key,
                match plan {
                    Ok(plan) => json!(plan),
                    Err(error) => json!({"unprepared": error.to_string()}),
                },
            );
        }
    }
    Value::Object(result)
}

#[test]
fn store_diet_baseline_plans() {
    let (temp, root, files) = fixture();
    let legacy = CallGraphStore::open(temp.path().join("legacy"), root.clone()).unwrap();
    legacy.cold_build(&files).unwrap();
    let views = view_store(temp.path(), &root, &files);
    let actual = json!({"legacy": plans(&legacy), "views": plans(&views)});
    golden(
        "baseline-plans.json",
        &serde_json::to_vec_pretty(&actual).unwrap(),
    );
}

fn sample_queries(store: &CallGraphStore) -> Vec<(String, String)> {
    let conn = store.conn.lock().unwrap();
    let mut statement = conn
        .prepare("SELECT file_path, scoped_name FROM nodes WHERE scoped_name <> '<top-level>'")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    let mut symbols = rows
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .unwrap()
        .into_iter()
        .collect::<Vec<_>>();
    symbols.sort_by_cached_key(|(file, symbol)| {
        blake3::hash(format!("callgraph-store-diet-v1\0{file}\0{symbol}").as_bytes())
            .as_bytes()
            .to_owned()
    });
    symbols
}

fn answer<T: serde::Serialize>(result: std::result::Result<T, impl std::fmt::Display>) -> Value {
    match result {
        Ok(value) => serde_json::to_value(value).unwrap(),
        Err(error) => json!({"error": error.to_string()}),
    }
}

fn seeded_replies(store: &CallGraphStore, root: &Path) -> Value {
    let symbols = sample_queries(store);
    assert!(!symbols.is_empty());
    let mut results = Vec::new();
    for (file, symbol) in &symbols[..symbols.len().min(500)] {
        let path = root.join(file);
        results.push(json!({
            "file": file, "symbol": symbol,
            "callers": answer(adapter::callers_result(store, &path, symbol, 4, true)),
            "call_tree": answer(adapter::call_tree_result(store, &path, symbol, 4, true)),
            "impact": answer(adapter::impact_result(store, &path, symbol, 4, true)),
        }));
    }
    for i in 0..symbols.len().min(100) {
        let (file, symbol) = &symbols[i];
        let (to_file, to_symbol) = &symbols[(i + 1) % symbols.len()];
        let path = root.join(file);
        let cache = Arc::new(RwLock::new(SymbolCache::new()));
        results.push(json!({
            "file": file, "symbol": symbol, "to_file": to_file, "to_symbol": to_symbol,
            "trace_to": answer(adapter::trace_to_result(store, &path, symbol, 4, true)),
            "trace_to_symbol": answer(adapter::trace_to_symbol_result(store, &path, symbol, to_symbol, Some(&root.join(to_file)), 4, true)),
            "trace_data": answer(adapter::trace_data_result(store, &path, symbol, "value", 4, cache)),
        }));
    }
    json!(results)
}

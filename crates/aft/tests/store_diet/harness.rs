//! Offline storage parity oracle. Recording is explicit; ordinary tests only compare.
use super::*;
use crate::commands::callgraph_store_adapter as adapter;
use crate::parser::SymbolCache;
use serde_json::{json, Value};
use std::fs;
use std::sync::RwLock;

use crate::test_support::manifest_dir;

fn assets() -> PathBuf {
    manifest_dir().join("tests/store_diet")
}

fn read_capture(path: impl AsRef<Path>) -> Vec<u8> {
    use futures_util::FutureExt;
    use tokio::io::AsyncReadExt;

    let path = path.as_ref();
    let mut compressed = path.as_os_str().to_os_string();
    compressed.push(".gz");
    let compressed = PathBuf::from(compressed);
    let path = if path.extension().is_some_and(|extension| extension == "gz") {
        assert!(
            !(path.exists() && path.with_extension("").exists()),
            "ambiguous capture: {}",
            path.display()
        );
        path
    } else if compressed.exists() {
        assert!(!path.exists(), "ambiguous capture: {}", path.display());
        &compressed
    } else {
        path
    };
    let bytes = fs::read(path).unwrap();
    if path.extension().is_some_and(|extension| extension == "gz") {
        let mut decoder = async_compression::tokio::bufread::GzipDecoder::new(bytes.as_slice());
        decoder.multiple_members(true);
        let mut decoded = Vec::new();
        decoder
            .read_to_end(&mut decoded)
            .now_or_never()
            .expect("in-memory gzip decoder must not wait for I/O")
            .unwrap();
        decoded
    } else {
        bytes
    }
}

fn write_capture(path: &Path, bytes: &[u8]) {
    // The recorder already runs in Python. Use its canonical gzip writer for
    // both Rust and Python captures, so re-recording uses identical headers/codecs.
    let python = std::env::var_os("AFT_STORE_DIET_PYTHON")
        .expect("record.py must supply its Python interpreter");
    let mut child = std::process::Command::new(python)
        .arg(assets().join("capture_io.py"))
        .arg("write")
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    std::io::Write::write_all(&mut input, bytes).unwrap();
    drop(input);
    assert!(child.wait().unwrap().success());
}

fn golden(name: &str, bytes: &[u8]) {
    let path = assets().join(name);
    if std::env::var_os("AFT_STORE_DIET_RECORD").as_deref() == Some(std::ffi::OsStr::new("1")) {
        // Recording is allowed only in the named main checkout prepared by the
        // recorder, never in an encoding candidate that inherited this switch.
        static RECORD_BASELINE: OnceLock<()> = OnceLock::new();
        RECORD_BASELINE.get_or_init(|| {
            let expected = std::env::var("AFT_STORE_DIET_BASELINE")
                .expect("record.py --baseline SHA is required");
            let metadata: Value =
                serde_json::from_slice(&fs::read(assets().join("baseline.json")).unwrap()).unwrap();
            assert_eq!(metadata["code_revision"].as_str(), Some(expected.as_str()));
            let output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(assets())
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
        });
        write_capture(&path, bytes);
    } else {
        let recorded = read_capture(&path);
        // A candidate's differing capture can be kept for review (never as a
        // golden): AFT_STORE_DIET_CANDIDATE_DIR names a folder to write it to.
        if recorded != bytes {
            if let Some(directory) = std::env::var_os("AFT_STORE_DIET_CANDIDATE_DIR") {
                let directory = PathBuf::from(directory);
                fs::create_dir_all(&directory).unwrap();
                fs::write(directory.join(name.trim_end_matches(".gz")), bytes).unwrap();
            }
        }
        assert_eq!(recorded, bytes, "{} differs", path.display());
    }
}

fn fixture_files(root: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, files);
            } else if entry.file_type().unwrap().is_file() {
                let relative = relative_path(root, &path);
                if crate::parser::detect_language(&path).is_some()
                    || crate::callgraph_extractor::is_resolution_input(relative.as_bytes())
                {
                    files.push(path);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort();
    files
}

/// Each command's reply on the fixture. `impact` and `trace_to` are left
/// out: they label the functions where their paths start (`main`, a test,
/// an entry attribute, public API) from per-symbol facts that only the data
/// layer's call graph stores (`EntryFacts`). The SQLite store and views
/// built here store none, so those two replies would pin labels these
/// stores cannot give.
fn replies(store: &impl CallGraphRead, root: &Path) -> Value {
    let file = root.join("main.ts");
    let target = root.join("helpers.ts");
    let cache = Arc::new(RwLock::new(SymbolCache::new()));
    json!({
        "callers": adapter::callers_result(store, &target, "validate", 4, true).unwrap(),
        "call_tree": adapter::call_tree_result(store, &file, "main", 4, true).unwrap(),
        "trace_to_symbol": adapter::trace_to_symbol_result(store, &file, "main", "validate", Some(&target), 4, true).unwrap(),
        "trace_data": adapter::trace_data_result(store, &root.join("data_flow.ts"), "transformData", "rawInput", 4, cache).unwrap(),
    })
}

fn counts(store: &CallGraphStore) -> Value {
    let conn = store.conn.lock().unwrap();
    let tables: Vec<Value> =
        serde_json::from_slice(&read_capture(assets().join("table_encoding.json"))).unwrap();
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
    let root = manifest_dir()
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
        &serde_json::to_vec_pretty(&seeded_replies(&store, &store, &root)).unwrap(),
    );
}

struct TestViewBuild {
    store: CallGraphStore,
    shapes: Value,
    entries: Vec<(crate::views::RelPath, crate::views::ManifestEntry)>,
    blobs: PathBuf,
}

fn plans(store: &CallGraphStore) -> Value {
    let rows: Vec<Value> =
        serde_json::from_slice(&read_capture(assets().join("statements.json"))).unwrap();
    let conn = store.conn.lock().unwrap();
    // Other databases and transaction-local tables do not live in a finished
    // graph fixture. Keep their schemas on a separate in-memory connection.
    // Actual graph plans always use the real fixture's schema first.
    let auxiliary = Connection::open_in_memory().unwrap();
    for connection in [&*conn, &auxiliary] {
        connection
            .create_scalar_function(
                "aft_is_test_file",
                1,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8
                    | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
                |ctx| Ok(crate::inspect::job::is_test_file(&ctx.get::<String>(0)?)),
            )
            .unwrap();
    }
    let mut schema = conn.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND type IN ('table','index') ORDER BY type DESC").unwrap();
    for sql in schema.query_map([], |row| row.get::<_, String>(0)).unwrap() {
        let _ = auxiliary.execute_batch(&sql.unwrap());
    }
    let tables: Vec<Value> =
        serde_json::from_slice(&read_capture(assets().join("table_encoding.json"))).unwrap();
    for table in tables {
        let declaration = regex::Regex::new(&format!(r"(?i)^\s*(?:--[^\n]*\n\s*)*CREATE\s+(?:TEMP\s+)?TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?{}\s*\(", regex::escape(table["table"].as_str().unwrap()))).unwrap();
        if let Some(row) = rows
            .iter()
            .find(|row| row["id"] == table["schema_statement"])
        {
            for variant in row["variants"].as_array().unwrap() {
                let sql = variant["sql"].as_str().unwrap();
                if declaration.is_match(sql) {
                    let _ = auxiliary.execute_batch(sql);
                }
            }
        }
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
            if sql.contains("aft_is_test_file(") {
                assert!(
                    plan.is_ok(),
                    "test-file classifier query must prepare: {plan:?}"
                );
            }
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

/// Replies to sampled queries. Like [`replies`], it leaves `impact` and
/// `trace_to` out.
fn seeded_replies(store: &CallGraphStore, reader: &impl CallGraphRead, root: &Path) -> Value {
    let symbols = sample_queries(store);
    if symbols.is_empty() {
        return json!([]);
    }
    let mut results = Vec::new();
    for (file, symbol) in &symbols[..symbols.len().min(500)] {
        let path = root.join(file);
        results.push(json!({
            "file": file, "symbol": symbol,
            "callers": answer(adapter::callers_result(reader, &path, symbol, 4, true)),
            "call_tree": answer(adapter::call_tree_result(reader, &path, symbol, 4, true)),
        }));
    }
    let count = symbols.len() as u128;
    let total = count * count;
    let seed = blake3::hash(b"callgraph-store-diet-v1\0pairs");
    let start = u128::from_le_bytes(seed.as_bytes()[..16].try_into().unwrap()) % total;
    for i in 0..total.min(100) {
        let index = (start + i * (count + 1)) % total;
        let (file, symbol) = &symbols[(index / count) as usize];
        let (to_file, to_symbol) = &symbols[(index % count) as usize];
        let path = root.join(file);
        results.push(json!({
            "file": file, "symbol": symbol, "to_file": to_file, "to_symbol": to_symbol,
            "trace_to_symbol": answer(adapter::trace_to_symbol_result(reader, &path, symbol, to_symbol, Some(&root.join(to_file)), 4, true)),
        }));
    }
    // Data-flow scopes come from source-addressable selectors, not synthetic
    // graph scopes or overload storage suffixes. This selects inputs; no reply
    // field or error message is normalized.
    let cache = Arc::new(RwLock::new(SymbolCache::new()));
    let origins = symbols
        .iter()
        .filter(|(file, symbol)| {
            adapter::store_diet_trace_data_origin(&root.join(file), symbol, &cache)
        })
        .collect::<Vec<_>>();
    let expressions = [
        "value", "input", "rawInput", "x", "y", "data", "result", "items", "callback", "self",
        "this",
    ];
    let mut data_pairs = origins[..origins.len().min(100)]
        .iter()
        .flat_map(|(file, symbol)| {
            expressions
                .iter()
                .map(move |expression| (file, symbol, *expression))
        })
        .collect::<Vec<_>>();
    data_pairs.sort_by_cached_key(|(file, symbol, expression)| {
        *blake3::hash(
            format!("callgraph-store-diet-v1\0data\0{file}\0{symbol}\0{expression}").as_bytes(),
        )
        .as_bytes()
    });
    for (file, symbol, expression) in &data_pairs[..data_pairs.len().min(100)] {
        results.push(json!({"file": file, "symbol": symbol, "expression": expression,
            "trace_data": answer(adapter::trace_data_result(reader, &root.join(file), symbol, expression, 4, cache.clone()))}));
    }
    json!(results)
}

fn stored_values(store: &CallGraphStore) -> Value {
    // These are logical text values. Integer codecs must decode them before
    // comparison rather than changing the baseline vocabulary oracle.
    let conn = store.conn.lock().unwrap();
    let mut result = serde_json::Map::new();
    for (table, columns) in [
        ("files", vec!["lang"]),
        ("nodes", vec!["kind", "provenance"]),
        ("refs", vec!["kind", "status", "import_kind", "provenance"]),
        ("edges", vec!["kind", "provenance"]),
        ("dispatch_hints", vec!["provenance"]),
        ("backend_file_state", vec!["backend", "status"]),
        ("view_dispatch_sites", vec!["language"]),
    ] {
        let present: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        if !present {
            continue;
        }
        for column in columns {
            let mut statement = conn.prepare(&format!("SELECT {column}, count(*) FROM {table} WHERE {column} IS NOT NULL GROUP BY {column}")).unwrap();
            let values = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()
                .unwrap();
            result.insert(format!("{table}.{column}"), json!(values));
        }
    }
    for (table, column) in [("nodes", "id"), ("refs", "ref_id"), ("edges", "edge_id")] {
        let mut statement = conn
            .prepare(&format!("SELECT {column} FROM {table}"))
            .unwrap();
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        let mut forms = BTreeMap::<String, usize>::new();
        for id in ids {
            let id = id.unwrap();
            let form = [
                "view:symbol:pos:symbol:",
                "view:symbol:top:",
                "pos:symbol:",
                "top:",
                "edge:view:",
                "dispatch:",
                "view:",
            ]
            .into_iter()
            .find(|prefix| id.starts_with(prefix))
            .unwrap_or("digest");
            *forms.entry(form.to_string()).or_default() += 1;
        }
        result.insert(format!("{table}.id_form"), json!(forms));
    }
    let mut statement = conn.prepare("SELECT kind, provenance, caller_node IS NOT NULL, full_ref IS NOT NULL, module_path IS NOT NULL, import_kind IS NOT NULL, wildcard, target_node IS NOT NULL, target_file IS NOT NULL, target_symbol IS NOT NULL FROM refs WHERE status='unresolved'").unwrap();
    let rows = statement
        .query_map([], |row| {
            let flags = (2..10)
                .map(|i| row.get::<_, i64>(i))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(format!(
                "{}:{}:{flags:?}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?
            ))
        })
        .unwrap();
    let mut forms = BTreeMap::<String, usize>::new();
    for form in rows {
        *forms.entry(form.unwrap()).or_default() += 1;
    }
    result.insert("refs.unresolved_form".into(), json!(forms));
    let orphan_edges: i64 = conn.query_row("SELECT count(*) FROM edges e LEFT JOIN refs r ON r.ref_id=e.ref_id WHERE r.ref_id IS NULL", [], |row| row.get(0)).unwrap();
    result.insert("edges.without_ref_row".into(), json!(orphan_edges));
    Value::Object(result)
}

fn corpus_cases() -> Vec<Value> {
    serde_json::from_slice(&read_capture(assets().join("corpus_manifest.json"))).unwrap()
}

#[test]
fn store_diet_language_corpus_legacy() {
    let temp = tempfile::tempdir().unwrap();
    let mut results = serde_json::Map::new();
    for case in corpus_cases() {
        let name = case["name"].as_str().unwrap();
        let root = assets().join("corpus").join(name).canonicalize().unwrap();
        let files = fixture_files(&root);
        let store = CallGraphStore::open(temp.path().join(name), root.clone()).unwrap();
        store.cold_build(&files).unwrap();
        results.insert(name.to_string(), json!({"counts": counts(&store), "values": stored_values(&store), "queries": seeded_replies(&store, &store, &root)}));
    }
    golden(
        "corpus-legacy.json",
        &serde_json::to_vec_pretty(&results).unwrap(),
    );
}

/// Replays every query the views corpus golden recorded on main through the
/// data layer's call graph, case by case. The golden pins main's answers;
/// the data layer replaces the views store, so a reply may differ from main
/// only where `corpus-data-layer-differences.json` lists it, with the
/// reason, and the reply the data layer gives instead. Any other difference
/// fails, as does a listed difference that no longer occurs or whose reply
/// changed.
#[test]
fn store_diet_language_corpus_data_layer_answers() {
    let golden: Value =
        serde_json::from_slice(&read_capture(assets().join("corpus-views.json"))).unwrap();
    let reviewed: Vec<Value> = serde_json::from_slice(&read_capture(
        assets().join("corpus-data-layer-differences.json"),
    ))
    .unwrap();
    let key = |entry: &Value| {
        ["case", "operation", "file", "symbol", "to", "expression"].map(|field| {
            entry
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
    };
    let mut expected = reviewed
        .iter()
        .map(|entry| {
            assert!(
                entry["reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.is_empty()),
                "a listed difference must give its reason: {entry}"
            );
            (key(entry), entry["data_layer"].clone())
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        expected.len(),
        reviewed.len(),
        "a difference is listed twice"
    );
    let mut differences = Vec::new();
    let mut compared = 0usize;
    for case in corpus_cases() {
        let name = case["name"].as_str().unwrap();
        let root = assets().join("corpus").join(name).canonicalize().unwrap();
        let reader = crate::data_layer::trace_measure::memory_reader(&root);
        let cache = Arc::new(RwLock::new(SymbolCache::new()));
        for recorded in golden[name]["queries"].as_array().into_iter().flatten() {
            let file = recorded["file"].as_str().unwrap();
            let symbol = recorded["symbol"].as_str().unwrap();
            let path = root.join(file);
            let mut replies = Vec::new();
            if let Some(to_symbol) = recorded["to_symbol"].as_str() {
                let to_file = root.join(recorded["to_file"].as_str().unwrap());
                replies.push((
                    "trace_to_symbol",
                    answer(adapter::trace_to_symbol_result(
                        &reader,
                        &path,
                        symbol,
                        to_symbol,
                        Some(&to_file),
                        4,
                        true,
                    )),
                ));
            } else if let Some(expression) = recorded["expression"].as_str() {
                replies.push((
                    "trace_data",
                    answer(adapter::trace_data_result(
                        &reader,
                        &path,
                        symbol,
                        expression,
                        4,
                        cache.clone(),
                    )),
                ));
            } else {
                replies.push((
                    "callers",
                    answer(adapter::callers_result(&reader, &path, symbol, 4, true)),
                ));
                replies.push((
                    "call_tree",
                    answer(adapter::call_tree_result(&reader, &path, symbol, 4, true)),
                ));
            }
            for (operation, actual) in replies {
                compared += 1;
                let found = json!({
                    "case": name, "file": file, "symbol": symbol,
                    "to": recorded.get("to_symbol"), "expression": recorded.get("expression"),
                    "operation": operation, "main": recorded[operation], "data_layer": actual,
                });
                match expected.remove(&key(&found)) {
                    // Listed: the data layer must give the reviewed reply,
                    // and it must still differ from main's.
                    Some(reviewed) if reviewed == actual && recorded[operation] != actual => {}
                    None if recorded[operation] == actual => {}
                    _ => differences.push(found),
                }
            }
        }
    }
    for difference in &differences {
        println!("DIFFERENCE {difference}");
    }
    println!(
        "compared {compared} replies; {} differ from main and the reviewed list",
        differences.len()
    );
    assert!(compared > 0, "no recorded query was replayed");
    assert!(
        expected.is_empty(),
        "listed differences no recorded query reaches: {:?}",
        expected.keys().collect::<Vec<_>>()
    );
    assert!(
        differences.is_empty(),
        "{} replies differ from main's recorded answers and the reviewed list",
        differences.len()
    );
}

static VALUE_PROBE_TRACE: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

#[test]
fn store_diet_transient_and_backend_values() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let good = root.join("good.rs");
    let bad = root.join("bad.rs");
    fs::write(&good, "fn f() {}\nfn g() { f(); }\n").unwrap();
    // A single invalid UTF-8 byte is the smallest input that exercises the
    // undecodable backend state; valid language programs cannot produce it.
    fs::write(&bad, [0xff]).unwrap();
    let store = CallGraphStore::open(temp.path().join("store"), root).unwrap();
    VALUE_PROBE_TRACE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .clear();
    store.conn.lock().unwrap().trace(Some(|sql| {
        // Installing a local observer replaces SQLite's connection callback.
        // Forward the same bytes to the optional SQL trace log as one append.
        if let Some(path) = std::env::var_os("AFT_STORE_DIET_SQL_TRACE") {
            let mut bytes = serde_json::to_vec(sql).unwrap();
            bytes.push(b'\n');
            let mut log = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            std::io::Write::write_all(&mut log, &bytes).unwrap();
        }
        VALUE_PROBE_TRACE
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .push(sql.to_owned())
    }));
    store.cold_build(&[good.clone(), bad]).unwrap();
    let insert = regex::Regex::new(r"(?is)^\s*INSERT(?: OR \w+)? INTO refs\s*\(").unwrap();
    let staged = VALUE_PROBE_TRACE
        .get()
        .unwrap()
        .lock()
        .unwrap()
        .iter()
        .any(|sql| insert.is_match(sql) && sql.contains("'staged'"));
    assert!(
        staged,
        "the cold pipeline must actually write its staged ref status"
    );
    let before = stored_values(&store);
    store.mark_files_stale(&[good]).unwrap();
    let after = stored_values(&store);
    golden(
        "lifecycle-values.json",
        &serde_json::to_vec_pretty(
            &json!({"after_cold": before, "after_stale_mark": after, "staged_ref_write": staged}),
        )
        .unwrap(),
    );
}

fn ordered_dispatch_rows(store: &CallGraphStore) -> Value {
    let connection = store.conn.lock().unwrap();
    dispatch_rows(&connection)
}

fn dispatch_rows(connection: &Connection) -> Value {
    let mut result = serde_json::Map::new();
    for (table, columns, count) in [
        (
            "refs",
            "ref_id, kind, status, target_symbol, provenance",
            5usize,
        ),
        ("edges", "edge_id, ref_id, target_symbol, provenance", 4),
    ] {
        let mut statement = connection
            .prepare(&format!("SELECT {columns} FROM {table} ORDER BY rowid"))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                (0..count)
                    .map(|i| row.get::<_, Option<String>>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        result.insert(table.into(), json!(rows));
    }
    Value::Object(result)
}

fn reordered_site_manifest(
    blobs: &Path,
    mut entries: Vec<(crate::views::RelPath, crate::views::ManifestEntry)>,
    file: &[u8],
    duplicate: bool,
    reverse: bool,
) -> (
    crate::views::Manifest,
    Vec<(crate::views::RelPath, crate::views::ManifestEntry)>,
) {
    use crate::views::{Manifest, ManifestEntry};
    let connection = Connection::open(blobs).unwrap();
    for (path, entry) in &mut entries {
        if path.as_bytes() != file {
            continue;
        }
        let ManifestEntry::Regular { planes, .. } = entry else {
            panic!("regular fixture")
        };
        let old = blake3::Hash::from_hex(planes.callgraph.as_ref().unwrap()).unwrap();
        let payload: Vec<u8> = connection
            .query_row(
                "SELECT payload FROM blob_payloads WHERE full_key=?1",
                [old.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        let mut blob = join::CallgraphBlob::from_bytes(&payload).unwrap();
        let join::CallgraphBlob::Parse(parsed) = &mut blob else {
            panic!("parse fixture")
        };
        let sites = parsed
            .dispatch
            .sites
            .iter()
            .enumerate()
            .filter(|(_, site)| site.member.as_deref() == Some("m"))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        assert!(sites.len() >= 2);
        if duplicate {
            parsed.dispatch.sites[sites[1]].ordinal = parsed.dispatch.sites[sites[0]].ordinal;
        }
        parsed.dispatch.sites[sites[0]].member = Some("f".into());
        if reverse {
            parsed.dispatch.sites.swap(sites[0], sites[1]);
        }
        let payload = blob.to_bytes().unwrap();
        let key = blake3::hash(&payload);
        connection
            .execute(
                "INSERT OR IGNORE INTO blob_payloads VALUES (?1,?2,?3,1)",
                params![
                    key.as_bytes().as_slice(),
                    payload,
                    key.as_bytes().as_slice()
                ],
            )
            .unwrap();
        planes.callgraph = Some(key.to_hex().to_string());
    }
    (Manifest::new(entries.clone()).unwrap(), entries)
}

fn json_fact_schema(value: &Value, path: &str, schema: &mut BTreeMap<String, BTreeSet<String>>) {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    schema
        .entry(path.to_string())
        .or_default()
        .insert(kind.to_string());
    match value {
        Value::Object(fields) => {
            for (name, value) in fields {
                json_fact_schema(value, &format!("{path}.{name}"), schema);
            }
        }
        Value::Array(items) => {
            for value in items {
                json_fact_schema(value, &format!("{path}[]"), schema);
            }
        }
        _ => {}
    }
}

#[test]
fn store_diet_json_fact_schema_accepts_new_optional_fields() {
    let value = json!({"known": [], "future_optional": "package", "absent_optional": null});
    let mut schema = BTreeMap::new();
    json_fact_schema(&value, "$", &mut schema);
    assert_eq!(
        schema["$.future_optional"],
        BTreeSet::from(["string".to_string()])
    );
    assert_eq!(
        schema["$.absent_optional"],
        BTreeSet::from(["null".to_string()])
    );
}

#[test]
fn store_diet_gzip_reader_preserves_exact_bytes_and_all_members() {
    use futures_util::FutureExt;
    use tokio::io::AsyncReadExt;

    let encode = |bytes: &[u8]| {
        let mut encoder = async_compression::tokio::bufread::GzipEncoder::new(bytes);
        let mut compressed = Vec::new();
        encoder
            .read_to_end(&mut compressed)
            .now_or_never()
            .expect("in-memory gzip encoder must not wait for I/O")
            .unwrap();
        compressed
    };
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("capture.json");
    let compressed = temp.path().join("capture.json.gz");
    let expected = b" {\"rows\": [1, 2]} \n";
    fs::write(&path, expected).unwrap();
    assert_eq!(read_capture(&path), expected);
    fs::remove_file(&path).unwrap();
    let mut members = encode(expected);
    fs::write(&compressed, &members).unwrap();
    assert_eq!(read_capture(&path), expected);
    members.extend(encode(b"trailing member"));
    fs::write(&compressed, members).unwrap();
    assert_eq!(
        read_capture(&path),
        b" {\"rows\": [1, 2]} \ntrailing member"
    );
    fs::write(&compressed, encode(b" {\"rows\": [2, 1]} \n")).unwrap();
    assert_ne!(read_capture(&path), expected);
}

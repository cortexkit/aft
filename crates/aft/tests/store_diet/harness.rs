//! Offline storage parity oracle. Recording is explicit; ordinary tests only compare.
use super::*;
use crate::commands::callgraph_store_adapter as adapter;
use crate::parser::SymbolCache;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::sync::RwLock;

/// The crate folder, read when the test runs. The test runner remaps
/// `CARGO_MANIFEST_DIR` when a test binary built on one machine runs from
/// another checkout (CI builds the macOS archive on a different host), so a
/// path fixed at compile time would point at a folder that doesn't exist.
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

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

fn answer<T: serde::Serialize>(result: std::result::Result<T, impl std::fmt::Display>) -> Value {
    match result {
        Ok(value) => serde_json::to_value(value).unwrap(),
        Err(error) => json!({"error": error.to_string()}),
    }
}

fn corpus_cases() -> Vec<Value> {
    serde_json::from_slice(&read_capture(assets().join("corpus_manifest.json"))).unwrap()
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

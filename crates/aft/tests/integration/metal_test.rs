use std::fs;
use std::path::Path;

use aft::callgraph_store::CallGraphStore;
use aft::commands::callgraph_store_adapter;
use aft::parser::{detect_language, LangId};
use serde_json::{json, Value};
use tempfile::tempdir;

use super::helpers::AftProcess;

const METAL_SOURCE: &str = include_str!("../fixtures/sample.metal");

#[test]
fn metal_extension_outline_and_zoom_ride_the_cpp_grammar() {
    assert_eq!(
        detect_language(Path::new("compute.metal")),
        Some(LangId::Metal)
    );

    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let file = project_root.join("tests/fixtures/sample.metal");
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(project_root)["success"], true);

    let outline = aft.send(
        &json!({
            "id": "outline-metal",
            "command": "outline",
            "file": file,
        })
        .to_string(),
    );
    assert_eq!(outline["success"], true, "outline failed: {outline:?}");
    assert_eq!(
        outline["text"].as_str().expect("outline text"),
        "sample.metal\n  float brighten(float value) 4:6\n  kernel void brighten_buffer(device float *values [[buffer(0)]], uint id [[thread_position_in_grid]]) 8:10\n"
    );

    let zoom = aft.send(
        &json!({
            "id": "zoom-metal-kernel",
            "command": "zoom",
            "file": project_root.join("tests/fixtures/sample.metal"),
            "symbol": "brighten_buffer",
            "context_lines": 0,
        })
        .to_string(),
    );
    assert_eq!(zoom["success"], true, "zoom failed: {zoom:?}");
    assert_eq!(zoom["kind"], "function");
    assert_eq!(
        zoom["content"].as_str().expect("zoom content"),
        "kernel void brighten_buffer(device float *values [[buffer(0)]], uint id [[thread_position_in_grid]]) {\n    values[id] = brighten(values[id]);\n}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn metal_store_records_shader_to_helper_edge() {
    let dir = tempdir().expect("temporary Metal project");
    let root = fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let file = root.join("sample.metal");
    fs::write(&file, METAL_SOURCE).expect("write Metal fixture");

    let store = CallGraphStore::open(root.join(".callgraph-store"), root.clone())
        .expect("open callgraph store");
    store
        .cold_build(std::slice::from_ref(&file))
        .expect("build Metal callgraph");

    let response = response_json(callgraph_store_adapter::callers_result(
        &store, &file, "brighten", 1, true,
    ));
    let callers = response["callers"]
        .as_array()
        .expect("caller groups")
        .iter()
        .flat_map(|group| group["callers"].as_array().expect("group callers"))
        .collect::<Vec<_>>();
    assert_eq!(callers.len(), 1, "unexpected caller response: {response:#}");
    assert_eq!(
        callers[0]["symbol"], "brighten_buffer",
        "caller response: {response:#}"
    );
    assert!(
        callers[0]["resolved_by"].is_null(),
        "same-file call should resolve exactly: {response:#}"
    );
}

fn response_json<T: serde::Serialize>(
    result: Result<T, aft::callgraph_store::CallGraphStoreError>,
) -> Value {
    serde_json::to_value(result.expect("callgraph store query")).expect("serialize store response")
}

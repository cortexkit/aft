use std::fs;
use std::path::Path;

use aft::callgraph_store::CallGraphStore;
use aft::commands::callgraph_store_adapter;
use aft::parser::{detect_language, LangId};
use serde_json::{json, Value};
use tempfile::tempdir;

use super::helpers::AftProcess;

const CUDA_SOURCE: &str = include_str!("../fixtures/sample.cu");

#[test]
fn cuda_extensions_outline_and_zoom_use_the_cuda_grammar() {
    assert_eq!(detect_language(Path::new("kernel.cu")), Some(LangId::Cuda));
    assert_eq!(detect_language(Path::new("kernel.cuh")), Some(LangId::Cuda));

    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let file = project_root.join("tests/fixtures/sample.cu");
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(project_root)["success"], true);

    let outline = aft.send(
        &json!({
            "id": "outline-cuda",
            "command": "outline",
            "file": file,
        })
        .to_string(),
    );
    assert_eq!(outline["success"], true, "outline failed: {outline:?}");
    assert_eq!(
        outline["text"].as_str().expect("outline text"),
        "sample.cu\n  __device__ float scale(float value) 1:3\n  __global__ void transform(float *data) 5:8\n  void launch_transform(float *data, dim3 grid, dim3 block) 10:12\n"
    );

    let zoom = aft.send(
        &json!({
            "id": "zoom-cuda-kernel",
            "command": "zoom",
            "file": project_root.join("tests/fixtures/sample.cu"),
            "symbol": "transform",
            "context_lines": 0,
        })
        .to_string(),
    );
    assert_eq!(zoom["success"], true, "zoom failed: {zoom:?}");
    assert_eq!(zoom["kind"], "kernel");
    assert_eq!(
        zoom["content"].as_str().expect("zoom content"),
        "__global__ void transform(float *data) {\n    int index = threadIdx.x;\n    data[index] = scale(data[index]);\n}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn cuda_store_records_kernel_launch_and_device_helper_edges() {
    let dir = tempdir().expect("temporary CUDA project");
    let root = fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let file = root.join("sample.cu");
    fs::write(&file, CUDA_SOURCE).expect("write CUDA fixture");

    let store = CallGraphStore::open(root.join(".callgraph-store"), root.clone())
        .expect("open callgraph store");
    store
        .cold_build(std::slice::from_ref(&file))
        .expect("build CUDA callgraph");

    let kernel_callers = response_json(callgraph_store_adapter::callers_result(
        &store,
        &file,
        "transform",
        1,
        true,
    ));
    assert_exact_caller(&kernel_callers, "launch_transform");

    let helper_callers = response_json(callgraph_store_adapter::callers_result(
        &store, &file, "scale", 1, true,
    ));
    assert_exact_caller(&helper_callers, "transform");
}

fn response_json<T: serde::Serialize>(
    result: Result<T, aft::callgraph_store::CallGraphStoreError>,
) -> Value {
    serde_json::to_value(result.expect("callgraph store query")).expect("serialize store response")
}

fn assert_exact_caller(response: &Value, symbol: &str) {
    let callers = response["callers"]
        .as_array()
        .expect("caller groups")
        .iter()
        .flat_map(|group| group["callers"].as_array().expect("group callers"))
        .collect::<Vec<_>>();
    assert_eq!(callers.len(), 1, "unexpected caller response: {response:#}");
    assert_eq!(
        callers[0]["symbol"], symbol,
        "caller response: {response:#}"
    );
    assert!(
        callers[0]["resolved_by"].is_null(),
        "same-file call should resolve exactly: {response:#}"
    );
}

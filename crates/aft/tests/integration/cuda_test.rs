use std::path::Path;

use aft::parser::{detect_language, LangId};
use serde_json::json;

use super::helpers::AftProcess;

#[test]
fn cuda_extensions_outline_and_zoom_use_the_cuda_grammar() {
    assert_eq!(detect_language(Path::new("kernel.cu")), Some(LangId::Cuda));
    assert_eq!(detect_language(Path::new("kernel.cuh")), Some(LangId::Cuda));

    let project_root = crate::test_helpers::manifest_dir();
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

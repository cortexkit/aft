use std::path::Path;

use aft::parser::{detect_language, LangId};
use serde_json::json;

use super::helpers::AftProcess;

#[test]
fn metal_extension_outline_and_zoom_ride_the_cpp_grammar() {
    assert_eq!(
        detect_language(Path::new("compute.metal")),
        Some(LangId::Metal)
    );

    let project_root = crate::test_helpers::manifest_dir();
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

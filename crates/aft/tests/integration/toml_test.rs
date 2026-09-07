use serde_json::json;
use std::path::Path;

use aft::language::LanguageProvider;
use aft::parser::{detect_language, LangId, TreeSitterProvider};

use super::helpers::AftProcess;

#[test]
fn toml_extension_and_workspace_manifest_symbols_are_supported() {
    assert_eq!(
        detect_language(Path::new("pyproject.toml")),
        Some(LangId::Toml)
    );

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let provider = TreeSitterProvider::new();
    let symbols = provider
        .list_symbols(&manifest)
        .expect("extract symbols from the workspace Cargo manifest");

    let package = symbols
        .iter()
        .find(|symbol| symbol.name == "package")
        .expect("[package] section");
    assert!(package.range.end_line > package.range.start_line);
    assert!(symbols
        .iter()
        .any(|symbol| symbol.name == "name" && symbol.parent.as_deref() == Some("package")));
    assert!(symbols
        .iter()
        .any(|symbol| symbol.name == "dependencies"
            && symbol.range.end_line > symbol.range.start_line));
}

#[test]
fn toml_outline_and_zoom_render_sections_and_child_keys() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let file = project_root.join("tests/fixtures/pyproject.toml");
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(project_root)["success"], true);

    let outline = aft.send(
        &json!({
            "id": "outline-toml",
            "command": "outline",
            "file": file,
        })
        .to_string(),
    );
    assert_eq!(outline["success"], true, "outline failed: {outline:?}");
    assert_eq!(
        outline["text"].as_str().expect("outline text"),
        "pyproject.toml\n  - var  project 1:4\n    .- var  name 2:2\n    .- var  version 3:3\n    .- var  requires-python 4:4\n  - var  tool.pytest.ini_options 6:7\n    .- var  testpaths 7:7\n  - var  project.authors 9:11\n    .- var  name 10:10\n    .- var  email 11:11\n"
    );

    let zoom = aft.send(
        &json!({
            "id": "zoom-toml",
            "command": "zoom",
            "file": project_root.join("tests/fixtures/pyproject.toml"),
            "symbol": "tool.pytest.ini_options",
            "context_lines": 0,
        })
        .to_string(),
    );
    assert_eq!(zoom["success"], true, "zoom failed: {zoom:?}");
    assert_eq!(zoom["name"], "tool.pytest.ini_options");
    assert_eq!(zoom["kind"], "variable");
    assert_eq!(
        zoom["content"].as_str().expect("zoom content"),
        "[tool.pytest.ini_options]\ntestpaths = [\"tests\"]"
    );

    assert!(aft.shutdown().success());
}

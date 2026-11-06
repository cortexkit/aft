//! Integration tests for the Nix language column: outline, zoom, AST
//! search/replace, semantic extension policy, and the unsupported columns.

use serde_json::json;
use std::fs;
use std::path::Path;

use aft::callgraph_store::join::CallgraphBlob;
use aft::language::LanguageProvider;
use aft::parser::{detect_language, LangId, TreeSitterProvider};

use super::helpers::AftProcess;

fn write_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    for name in ["flake.nix", "nix_module.nix"] {
        let source = fs::read_to_string(crate::helpers::fixture_path(name)).expect("fixture");
        fs::write(dir.path().join(name), source).unwrap();
    }
    dir
}

fn outline_files(aft: &mut AftProcess, directory: &Path) -> serde_json::Value {
    aft.send(
        &json!({
            "id": "outline-nix-files",
            "command": "outline",
            "directory": directory,
            "files": true,
        })
        .to_string(),
    )
}

#[test]
fn nix_outline_zoom_and_language_label() {
    assert_eq!(detect_language(Path::new("flake.nix")), Some(LangId::Nix));

    let dir = write_project();
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let outline = aft.send(
        &json!({
            "id": "outline-nix-module",
            "command": "outline",
            "file": dir.path().join("nix_module.nix"),
        })
        .to_string(),
    );
    assert_eq!(outline["success"], true, "outline failed: {outline:?}");
    let text = outline["text"].as_str().expect("outline text");
    assert!(
        text.contains("options.services.demo.enable"),
        "dotted module option missing: {text}"
    );
    assert!(text.contains("config.services.demo"), "{text}");
    assert!(text.contains("port"), "nested key missing: {text}");

    let flake_outline = aft.send(
        &json!({
            "id": "outline-nix-flake",
            "command": "outline",
            "file": dir.path().join("flake.nix"),
        })
        .to_string(),
    );
    assert_eq!(flake_outline["success"], true, "{flake_outline:?}");
    let flake_text = flake_outline["text"].as_str().unwrap();
    assert!(
        flake_text.contains("devShells.x86_64-linux.default"),
        "{flake_text}"
    );
    assert!(
        flake_text.contains("outputs = inputs@"),
        "function binding must keep its name in the outline: {flake_text}"
    );

    let files = outline_files(&mut aft, dir.path());
    assert_eq!(files["success"], true, "{files:?}");
    for name in ["flake.nix", "nix_module.nix"] {
        let entry = files["files"]
            .as_array()
            .expect("files array")
            .iter()
            .find(|entry| entry["path"] == name)
            .unwrap_or_else(|| panic!("missing {name}: {files:?}"));
        assert_eq!(entry["language"], "nix", "{entry:?}");
        assert!(entry["symbols"].as_u64().unwrap_or(0) > 0, "{entry:?}");
    }

    let zoom = aft.send(
        &json!({
            "id": "zoom-nix",
            "command": "zoom",
            "file": dir.path().join("flake.nix"),
            "symbol": "outputs.devShells.x86_64-linux.default",
            "context_lines": 0,
        })
        .to_string(),
    );
    assert_eq!(zoom["success"], true, "zoom failed: {zoom:?}");
    assert_eq!(
        zoom["range"]["start_line"], 7,
        "zoom must resolve the nested binding: {zoom:?}"
    );
    assert!(
        zoom["content"]
            .as_str()
            .unwrap_or_default()
            .trim_start()
            .starts_with("devShells.x86_64-linux.default ="),
        "zoom must return the nested binding, not the whole outputs expression: {zoom:?}"
    );

    let missing = aft.send(
        &json!({
            "id": "zoom-nix-missing",
            "command": "zoom",
            "file": dir.path().join("flake.nix"),
            "symbol": "outputs.notThere",
        })
        .to_string(),
    );
    assert_eq!(missing["success"], false, "{missing:?}");
    assert_eq!(missing["code"], "symbol_not_found", "{missing:?}");

    assert!(aft.shutdown().success());
}

#[test]
fn nix_ast_search_and_replace_preserve_siblings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("ast.nix");
    fs::write(&file, "{ foo = true; bar = false; baz.qux = true; }\n").unwrap();

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let search = aft.send(
        &json!({
            "id": "ast-search-nix",
            "command": "ast_search",
            "pattern": "$NAME = true;",
            "lang": "nix",
            "paths": [file.display().to_string()],
        })
        .to_string(),
    );
    assert_eq!(search["success"], true, "search failed: {search:?}");
    let matches = search["matches"].as_array().expect("matches array");
    assert_eq!(matches.len(), 2, "{search:?}");

    let replace = aft.send(
        &json!({
            "id": "ast-replace-nix",
            "command": "ast_replace",
            "pattern": "$NAME = true;",
            "rewrite": "$NAME = false;",
            "lang": "nix",
            "paths": [file.display().to_string()],
            "dry_run": false,
        })
        .to_string(),
    );
    assert_eq!(replace["success"], true, "replace failed: {replace:?}");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "{ foo = false; bar = false; baz.qux = false; }\n"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn nix_unsupported_columns_report_no_edges() {
    let dir = write_project();
    let file = dir.path().join("nix_module.nix");
    let file_str = file.display().to_string();

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let import = aft.send(
        &json!({
            "id": "add-import-nix",
            "command": "add_import",
            "file": file_str,
            "module": "nixpkgs",
        })
        .to_string(),
    );
    assert_eq!(import["success"], false, "{import:?}");
    assert_eq!(import["code"], "unsupported_language", "{import:?}");

    let refactor = aft.send(
        &json!({
            "id": "extract-function-nix",
            "command": "extract_function",
            "file": file_str,
            "name": "demo",
            "start_line": 3,
            "end_line": 5,
        })
        .to_string(),
    );
    assert_eq!(refactor["success"], false, "{refactor:?}");
    assert_eq!(refactor["code"], "unsupported_language", "{refactor:?}");

    // Nix contributes no call edges, so a zoom on a nested flake output carries
    // neither outgoing nor incoming calls.
    let zoom = aft.send(
        &json!({
            "id": "zoom-nix-callgraph",
            "command": "zoom",
            "file": dir.path().join("flake.nix"),
            "symbol": "outputs.devShells.x86_64-linux.default",
        })
        .to_string(),
    );
    assert_eq!(zoom["success"], true, "{zoom:?}");
    assert_eq!(
        zoom["annotations"]["calls_out"],
        json!([]),
        "no outgoing call edges: {zoom:?}"
    );
    assert_eq!(
        zoom["annotations"]["called_by"],
        json!([]),
        "no incoming call edges: {zoom:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn nix_validation_stays_unsupported() {
    let dir = write_project();
    let file = dir.path().join("nix_module.nix");
    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let params = json!({
        "id": "write-nix-validate",
        "command": "write",
        "file": file.display().to_string(),
        "content": format!("{}\n# validated\n", fs::read_to_string(&file).unwrap()),
        "validate": "full",
    });

    let response = aft.send(&params.to_string());
    assert_eq!(response["success"], true, "{response:?}");
    assert_eq!(
        response["validate_skipped_reason"], "unsupported_language",
        "Nix has no checker support: {response:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn nix_semantic_extension_is_indexed() {
    // The semantic indexing policy accepts `.nix`; chunk collection itself is
    // covered by the unit test with the same name in semantic_index.rs, where
    // the collector is reachable without the census feature.
    let dir = write_project();
    assert!(aft::semantic_index::is_semantic_indexed_extension(
        &dir.path().join("flake.nix")
    ));
}

#[test]
fn nix_views_callgraph_payload_accepts_nix_label() {
    let dir = write_project();
    let source = fs::read_to_string(dir.path().join("flake.nix")).unwrap();

    // The views plane extracts a callgraph blob for every source file and fails
    // publication on an unknown language label; this is that exact call.
    let blob = CallgraphBlob::extract(&source, "nix", "aft-test")
        .expect("views callgraph extraction must accept the nix label");
    let bytes = blob.to_bytes().expect("serialize callgraph blob");
    assert!(!bytes.is_empty());

    match blob {
        CallgraphBlob::Parse(parse) => {
            assert_eq!(parse.language, "nix");
            assert!(parse.imports.is_empty(), "Nix has no import support");
            assert!(
                parse.refs.is_empty(),
                "Nix contributes no call edges: {:?}",
                parse.refs
            );
        }
        CallgraphBlob::Config(_) => panic!("a .nix source must produce a parse blob"),
    }
}

#[test]
fn nix_symbols_from_provider_use_dotted_paths() {
    let dir = write_project();
    let provider = TreeSitterProvider::new();
    let symbols = provider
        .list_symbols(&dir.path().join("nix_module.nix"))
        .expect("list Nix symbols");
    let enable = symbols
        .iter()
        .find(|symbol| symbol.name == "enable")
        .expect("nested enable");
    assert_eq!(enable.scope_chain, vec!["config.services.demo"]);
    assert_eq!(enable.parent.as_deref(), Some("config.services.demo"));
}

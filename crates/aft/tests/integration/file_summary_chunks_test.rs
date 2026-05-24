use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use aft::semantic_index::{build_file_summary_chunk, SemanticIndex, SemanticIndexFingerprint};
use aft::symbols::SymbolKind;

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, content).expect("write fixture");
    }
    temp_dir
}

fn build_index_with_captured_texts(
    project_root: &Path,
    files: &[PathBuf],
) -> (SemanticIndex, Vec<String>) {
    let captured = RefCell::new(Vec::new());
    let mut embed = |texts: Vec<String>| {
        captured.borrow_mut().extend(texts.iter().cloned());
        Ok::<Vec<Vec<f32>>, String>(texts.into_iter().map(|_| vec![1.0]).collect())
    };

    let index = SemanticIndex::build(project_root, files, &mut embed, 64).expect("build index");
    (index, captured.into_inner())
}

#[test]
fn index_ts_with_one_export_emits_file_summary_and_symbol_chunks() {
    let project = setup_project(&[(
        "src/index.ts",
        "/** Entry point for plugin activation. */\nexport function initializePlugin() {\n  return true;\n}\n",
    )]);
    let file = project.path().join("src/index.ts");

    let (index, embed_texts) =
        build_index_with_captured_texts(project.path(), std::slice::from_ref(&file));

    assert_eq!(index.len(), 2);
    assert!(embed_texts.iter().any(|text| text.contains(
        "file:src/index.ts kind:file-summary name:index parent:src doc:/** Entry point"
    )));
    assert!(embed_texts.iter().any(|text| text
        .contains("name:initializePlugin file:src/index.ts kind:function name:initializePlugin")));

    let results = index.search(&[1.0], 10);
    let summary = results
        .iter()
        .find(|result| matches!(result.kind, SymbolKind::FileSummary))
        .expect("file-summary result");
    assert_eq!(summary.file, file);
    assert_eq!(summary.name, "index");
    assert_eq!(summary.start_line, 0);
    assert_eq!(summary.end_line, 0);
    assert!(!summary.exported);
    assert!(summary
        .snippet
        .contains("Entry point for plugin activation"));
    assert!(results
        .iter()
        .any(|result| result.name == "initializePlugin"));
}

#[test]
fn tools_ts_with_many_exports_emits_only_symbol_chunks() {
    let mut source = String::new();
    for i in 0..8 {
        source.push_str(&format!(
            "export function tool{i}() {{\n  return {i};\n}}\n\n"
        ));
    }
    let project = setup_project(&[("src/tools.ts", &source)]);
    let file = project.path().join("src/tools.ts");

    let (index, embed_texts) = build_index_with_captured_texts(project.path(), &[file]);

    assert_eq!(index.len(), 8);
    assert!(!embed_texts
        .iter()
        .any(|text| text.contains("kind:file-summary")));
    assert!(!index
        .search(&[1.0], 20)
        .iter()
        .any(|result| matches!(result.kind, SymbolKind::FileSummary)));
}

#[test]
fn rust_mod_with_only_pub_use_emits_file_summary_with_empty_exports() {
    let project = setup_project(&[("src/foo/mod.rs", "pub use foo::Bar;\npub use baz::Qux;\n")]);
    let file = project.path().join("src/foo/mod.rs");

    let (index, embed_texts) =
        build_index_with_captured_texts(project.path(), std::slice::from_ref(&file));

    assert_eq!(index.len(), 1);
    let summary_text = embed_texts
        .iter()
        .find(|text| text.contains("kind:file-summary"))
        .expect("file-summary embed text");
    assert!(summary_text.contains("file:src/foo/mod.rs"));
    assert!(summary_text.contains("name:mod"));
    assert!(summary_text.contains("parent:src/foo"));
    assert!(summary_text.ends_with("exports:"));

    let results = index.search(&[1.0], 10);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].file, file);
    assert!(matches!(results[0].kind, SymbolKind::FileSummary));
}

#[test]
fn reindex_roundtrip_after_chunking_version_bump_is_deterministic() {
    let project = setup_project(&[(
        "src/index.ts",
        "export function initializePlugin() {\n  return true;\n}\n",
    )]);
    let storage = tempfile::tempdir().expect("create storage dir");
    let file = project.path().join("src/index.ts");
    let (mut index, _) = build_index_with_captured_texts(project.path(), &[file]);
    let fingerprint = SemanticIndexFingerprint {
        backend: "fastembed".to_string(),
        model: "all-MiniLM-L6-v2".to_string(),
        base_url: "none".to_string(),
        dimension: 1,
        chunking_version: 2,
    };
    index.set_fingerprint(fingerprint.clone());
    index.write_to_disk(storage.path(), "file-summary-roundtrip");

    let restored = SemanticIndex::read_from_disk(
        storage.path(),
        "file-summary-roundtrip",
        project.path(),
        false,
        Some(&fingerprint.as_string()),
    )
    .expect("read persisted index");

    assert_eq!(
        restored
            .fingerprint()
            .expect("fingerprint")
            .chunking_version,
        2
    );
    let original = result_fingerprint(index.search(&[1.0], 10));
    let roundtrip = result_fingerprint(restored.search(&[1.0], 10));
    assert_eq!(roundtrip, original);
    assert_eq!(
        roundtrip
            .iter()
            .filter(|(_, kind, _, _, _)| matches!(kind, SymbolKind::FileSummary))
            .count(),
        1
    );
}

#[test]
fn build_file_summary_chunk_uses_documented_fields_for_empty_inputs() {
    let project = tempfile::tempdir().expect("create project dir");
    let file = project.path().join("src/index.ts");
    fs::create_dir_all(file.parent().expect("parent")).expect("create parent");

    let chunk = build_file_summary_chunk(
        &file,
        project.path(),
        "const localOnly = true;\n",
        &[],
        &[None],
    );

    assert_eq!(chunk.file, file);
    assert_eq!(chunk.name, "index");
    assert!(matches!(chunk.kind, SymbolKind::FileSummary));
    assert_eq!(chunk.start_line, 0);
    assert_eq!(chunk.end_line, 0);
    assert!(!chunk.exported);
    assert_eq!(
        chunk.embed_text,
        "file:src/index.ts kind:file-summary name:index parent:src doc: exports:"
    );
    assert_eq!(chunk.snippet, "");
}

fn result_fingerprint(
    results: Vec<aft::semantic_index::SemanticResult>,
) -> Vec<(String, SymbolKind, u32, u32, bool)> {
    results
        .into_iter()
        .map(|result| {
            (
                result.name,
                result.kind,
                result.start_line,
                result.end_line,
                result.exported,
            )
        })
        .collect()
}

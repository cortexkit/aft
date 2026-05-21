use std::fs;
use std::path::{Path, PathBuf};

use aft::semantic_index::SemanticIndex;

fn write_source_fixture(project_root: &std::path::Path) -> PathBuf {
    let source_file = project_root.join("src/lib.rs");
    fs::create_dir_all(source_file.parent().expect("source parent")).expect("create src dir");
    fs::write(
        &source_file,
        "pub fn handle_request(token: &str) -> bool {\n    !token.is_empty()\n}\n\npub fn normalize_user_id(input: &str) -> String {\n    input.trim().to_lowercase()\n}\n",
    )
    .expect("write source file");
    source_file
}

fn build_empty_v6_bytes(dimension: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.push(6u8);
    bytes.extend_from_slice(&dimension.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes()); // entry_count
    bytes.extend_from_slice(&0u32.to_le_bytes()); // fingerprint_len
    bytes.extend_from_slice(&0u32.to_le_bytes()); // mtime_count
    bytes
}

fn unit_vector(dimension: usize, hot_index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; dimension];
    vector[hot_index] = 1.0;
    vector
}

#[test]
fn build_returns_backend_http_errors_verbatim() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = write_source_fixture(project.path());
    let files = vec![source_file];
    let mut embed = |_texts: Vec<String>| {
        Err::<Vec<Vec<f32>>, String>(
            "openai compatible request failed (HTTP 401): Unauthorized".to_string(),
        )
    };

    let error = match SemanticIndex::build(project.path(), &files, &mut embed, 16) {
        Err(error) => error,
        Ok(_) => panic!("expected backend HTTP error"),
    };

    assert_eq!(
        error,
        "openai compatible request failed (HTTP 401): Unauthorized"
    );
}

#[test]
fn build_returns_error_when_embedding_backend_returns_no_vectors() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = write_source_fixture(project.path());
    let files = vec![source_file];
    let mut embed = |_texts: Vec<String>| Ok::<Vec<Vec<f32>>, String>(vec![]);

    let error = match SemanticIndex::build(project.path(), &files, &mut embed, 16) {
        Err(error) => error,
        Ok(_) => panic!("expected empty-vector validation error"),
    };

    assert_eq!(error, "embedding backend returned no vectors for 3 inputs");
}

#[test]
fn build_returns_error_when_embedding_dimension_changes_across_batches() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = write_source_fixture(project.path());
    let files = vec![source_file];
    let mut call_count = 0usize;
    let mut embed = |_texts: Vec<String>| {
        call_count += 1;
        match call_count {
            1 => Ok::<Vec<Vec<f32>>, String>(vec![vec![1.0; 384]]),
            2 => Ok::<Vec<Vec<f32>>, String>(vec![vec![1.0; 512]]),
            _ => panic!("unexpected extra embedding batch"),
        }
    };

    let error = match SemanticIndex::build(project.path(), &files, &mut embed, 1) {
        Err(error) => error,
        Ok(_) => panic!("expected dimension mismatch validation error"),
    };

    assert_eq!(
        error,
        "embedding dimension changed across batches: expected 384, got 512"
    );
}

#[test]
fn build_accepts_high_dimension_embeddings_and_search_roundtrips() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = write_source_fixture(project.path());
    let files = vec![source_file.clone()];
    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(
            texts
                .into_iter()
                .map(|text| {
                    if text.contains("handle_request") {
                        unit_vector(4096, 0)
                    } else if text.contains("normalize_user_id") {
                        unit_vector(4096, 1)
                    } else {
                        unit_vector(4096, 2)
                    }
                })
                .collect(),
        )
    };

    let index = SemanticIndex::build(project.path(), &files, &mut embed, 16)
        .expect("4096-dimensional build should be accepted");
    assert_eq!(index.dimension(), 4096);

    let storage = tempfile::tempdir().expect("create storage dir");
    index.write_to_disk(storage.path(), "high-dim-project");
    let restored = SemanticIndex::read_from_disk(
        storage.path(),
        "high-dim-project",
        project.path(),
        false,
        None,
    )
    .expect("restore 4096-dimensional semantic index");

    let results = restored.search(&unit_vector(4096, 1), 1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "normalize_user_id");
    assert_eq!(results[0].file, source_file);
}

#[test]
fn build_rejects_unsupported_embedding_dimensions() {
    for dimension in [0usize, 4097] {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = write_source_fixture(project.path());
        let files = vec![source_file];
        let mut embed = move |texts: Vec<String>| {
            Ok::<Vec<Vec<f32>>, String>(texts.into_iter().map(|_| vec![1.0; dimension]).collect())
        };

        let error = SemanticIndex::build(project.path(), &files, &mut embed, 16)
            .expect_err("unsupported dimensions should be rejected during build");
        assert!(
            error.contains(&format!("invalid embedding dimension: {dimension}"))
                && error.contains("supported range is 1..=4096"),
            "error should include dimension and supported range: {error}"
        );
    }
}

#[test]
fn from_bytes_accepts_and_rejects_dimension_boundaries() {
    let index = SemanticIndex::from_bytes(&build_empty_v6_bytes(4096), Path::new("/"))
        .expect("4096 dimensions should deserialize");
    assert_eq!(index.dimension(), 4096);

    for dimension in [0u32, 4097] {
        let error = SemanticIndex::from_bytes(&build_empty_v6_bytes(dimension), Path::new("/"))
            .expect_err("unsupported dimension should be rejected");
        assert!(
            error.contains(&format!("invalid embedding dimension: {dimension}"))
                && error.contains("supported range is 1..=4096"),
            "error should include supported range: {error}"
        );
    }
}

#[test]
fn build_returns_error_when_embedding_backend_returns_too_few_vectors() {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = write_source_fixture(project.path());

    let files = vec![source_file];
    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(
            texts
                .into_iter()
                .skip(1)
                .map(|_| vec![1.0, 0.0, 0.0])
                .collect(),
        )
    };

    let error = match SemanticIndex::build(project.path(), &files, &mut embed, 16) {
        Err(error) => error,
        Ok(_) => panic!("expected vector count validation error"),
    };

    assert_eq!(error, "embedding backend returned 2 vectors for 3 inputs");
}

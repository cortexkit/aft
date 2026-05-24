use std::{fs, path::PathBuf};

use aft::compress::{builtin_filters, compress_with_registry, toml_filter};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct FixtureManifestEntry {
    file: String,
    command: String,
    category: String,
    tier: String,
}

#[derive(Debug, Serialize)]
struct SpikeOutputEntry {
    file: String,
    command: String,
    category: String,
    tier: String,
    original_bytes: usize,
    compressed_bytes: usize,
    original_text: String,
    compressed_text: String,
}

#[test]
fn compression_token_spike_emits_fixture_outputs() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/aft has repository root two levels up");
    let benchmark_dir = repo_root.join("benchmarks/compression-tokens");
    let fixtures_dir = benchmark_dir.join("fixtures");
    let manifest_path = fixtures_dir.join("manifest.json");

    let manifest_text = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
    let manifest: Vec<FixtureManifestEntry> = serde_json::from_str(&manifest_text)
        .unwrap_or_else(|e| panic!("parse {}: {e}", manifest_path.display()));

    let registry = toml_filter::build_registry(builtin_filters::ALL, None, None);
    let mut outputs = Vec::with_capacity(manifest.len());

    for entry in manifest {
        let fixture_path = fixtures_dir.join(&entry.file);
        let original_text = fs::read_to_string(&fixture_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", fixture_path.display()));
        let compressed_text = compress_with_registry(&entry.command, &original_text, &registry);

        outputs.push(SpikeOutputEntry {
            file: entry.file,
            command: entry.command,
            category: entry.category,
            tier: entry.tier,
            original_bytes: original_text.len(),
            compressed_bytes: compressed_text.len(),
            original_text,
            compressed_text,
        });
    }

    let data_dir = benchmark_dir.join("data");
    fs::create_dir_all(&data_dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", data_dir.display()));
    let output_path = data_dir.join("spike-output.json");
    let json = serde_json::to_string_pretty(&outputs).expect("serialize spike output");
    fs::write(&output_path, json)
        .unwrap_or_else(|e| panic!("write {}: {e}", output_path.display()));
}

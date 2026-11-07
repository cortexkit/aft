use aft::callgraph_store::join::{
    CallgraphBlob, ManifestBlobReader, ManifestJoinError, ResolutionStatus,
};
use aft::views::{Manifest, ManifestEntry, RegularPlanes, RelPath};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct MemoryBlobs(BTreeMap<String, Vec<u8>>);

impl ManifestBlobReader for MemoryBlobs {
    fn read_callgraph_blob(&self, key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        Ok(self.0.get(key).cloned())
    }
}

#[test]
fn rust_crate_roots_resolve_bin_and_other_target_module_trees() {
    let root = crate::helpers::fixture_path("callgraph/rust_bin_targets");
    let (manifest, blobs) = manifest_fixture(&root);
    let joined = aft::callgraph_store::join::JoinResult::from_manifest(&manifest, &blobs).unwrap();

    let exact_tool_sites = BTreeSet::from([
        (
            "src/bin/support/admin_client.rs".to_string(),
            4_i64,
            "route_client::error_reason".to_string(),
        ),
        (
            "src/bin/support/admin_client.rs".to_string(),
            8,
            "route_client::error_reason".to_string(),
        ),
        (
            "src/bin/support/admin_client.rs".to_string(),
            12,
            "super::route_client::error_reason".to_string(),
        ),
        (
            "src/bin/support/admin_client.rs".to_string(),
            16,
            "crate::support::route_client::error_reason".to_string(),
        ),
        (
            "src/bin/support/route_client.rs".to_string(),
            6,
            "error_reason".to_string(),
        ),
    ]);
    assert_eq!(
        resolved_sites(
            &joined,
            &blobs,
            "src/bin/support/route_client.rs",
            "error_reason"
        ),
        exact_tool_sites,
        "qualified bin calls must become precise resolver edges"
    );
    assert!(
        resolved_sites(&joined, &blobs, "src/route_client.rs", "error_reason").is_empty(),
        "the library decoy must not receive bin-target calls"
    );

    assert_target_site(
        &joined,
        &blobs,
        "src/bin/other/nested.rs",
        "nested_target",
        "src/bin/other/main.rs",
        4,
        "crate::nested::nested_target",
    );
    assert_target_site(
        &joined,
        &blobs,
        "tools/custom_support.rs",
        "custom_target",
        "tools/custom.rs",
        4,
        "custom_support::custom_target",
    );
    assert_target_site(
        &joined,
        &blobs,
        "examples/demo_support.rs",
        "example_target",
        "examples/demo.rs",
        4,
        "demo_support::example_target",
    );
    assert_target_site(
        &joined,
        &blobs,
        "tests/test_support.rs",
        "integration_target",
        "tests/standalone.rs",
        5,
        "test_support::integration_target",
    );
    assert_target_site(
        &joined,
        &blobs,
        "benches/bench_support.rs",
        "bench_target",
        "benches/perf.rs",
        4,
        "bench_support::bench_target",
    );
    assert_unresolved_site(
        &joined,
        &blobs,
        "src/binx/caller.rs",
        2,
        "crate::support::route_client::error_reason",
    );

    assert_target_site(
        &joined,
        &blobs,
        "src/lib.rs",
        "lib_target",
        "src/lib.rs",
        9,
        "super::lib_target",
    );

    let view_sites = joined
        .rows
        .iter()
        .filter(|row| {
            row.target_path.as_deref() == Some(b"src/bin/support/route_client.rs")
                && row.target_symbol.as_deref() == Some("error_reason")
        })
        .map(|row| {
            let blob = CallgraphBlob::from_bytes(&blobs.0[&row.caller_blob_key]).unwrap();
            let raw = blob
                .parse()
                .unwrap()
                .refs
                .iter()
                .find(|raw| raw.ordinal == row.ref_ordinal)
                .unwrap();
            (
                String::from_utf8(row.caller_path.clone()).unwrap(),
                i64::from(raw.line),
                raw.full_ref.clone().unwrap(),
                row.status,
            )
        })
        .collect::<BTreeSet<_>>();
    let expected_view_sites = exact_tool_sites
        .into_iter()
        .map(|(file, line, text)| (file, line, text, ResolutionStatus::Resolved))
        .collect();
    assert_eq!(
        view_sites, expected_view_sites,
        "ProjectFacts resolver must match disk resolution"
    );
}

fn assert_target_site(
    joined: &aft::callgraph_store::join::JoinResult,
    blobs: &MemoryBlobs,
    target_file: &str,
    target_symbol: &str,
    caller_file: &str,
    line: i64,
    call_text: &str,
) {
    assert_eq!(
        resolved_sites(joined, blobs, target_file, target_symbol),
        BTreeSet::from([(caller_file.to_string(), line, call_text.to_string())]),
        "expected precise caller for {target_file}:{target_symbol}"
    );
}

fn assert_unresolved_site(
    joined: &aft::callgraph_store::join::JoinResult,
    blobs: &MemoryBlobs,
    caller_file: &str,
    line: i64,
    call_text: &str,
) {
    let row = joined
        .rows
        .iter()
        .find(|row| {
            if row.caller_path != caller_file.as_bytes() {
                return false;
            }
            let blob = CallgraphBlob::from_bytes(&blobs.0[&row.caller_blob_key]).unwrap();
            let raw = blob
                .parse()
                .unwrap()
                .refs
                .iter()
                .find(|raw| raw.ordinal == row.ref_ordinal)
                .unwrap();
            i64::from(raw.line) == line && raw.full_ref.as_deref() == Some(call_text)
        })
        .expect("reference row");
    assert_eq!(row.status, ResolutionStatus::Unresolved);
    assert_eq!(row.target_path, None);
}

fn resolved_sites(
    joined: &aft::callgraph_store::join::JoinResult,
    blobs: &MemoryBlobs,
    target_file: &str,
    target_symbol: &str,
) -> BTreeSet<(String, i64, String)> {
    joined
        .rows
        .iter()
        .filter(|row| {
            row.target_path.as_deref() == Some(target_file.as_bytes())
                && row.target_symbol.as_deref() == Some(target_symbol)
                && row.status == ResolutionStatus::Resolved
        })
        .map(|row| {
            let blob = CallgraphBlob::from_bytes(&blobs.0[&row.caller_blob_key]).unwrap();
            let raw = blob
                .parse()
                .unwrap()
                .refs
                .iter()
                .find(|raw| raw.ordinal == row.ref_ordinal)
                .unwrap();
            (
                String::from_utf8(row.caller_path.clone()).unwrap(),
                i64::from(raw.line),
                raw.full_ref.clone().unwrap(),
            )
        })
        .collect()
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, files);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    collect(root, &mut files);
    files.sort();
    files
}

fn manifest_fixture(root: &Path) -> (Manifest, MemoryBlobs) {
    let mut paths = rust_files(root);
    paths.push(root.join("Cargo.toml"));
    paths.sort();
    let mut blobs = MemoryBlobs::default();
    let mut entries = Vec::new();
    for path in paths {
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let source = fs::read_to_string(&path).unwrap();
        let blob = if rel == "Cargo.toml" {
            CallgraphBlob::config(source, "rust-bin-target-test")
        } else {
            CallgraphBlob::extract(&source, "rust", "rust-bin-target-test").unwrap()
        };
        let bytes = blob.to_bytes().unwrap();
        let key = blake3::hash(&bytes).to_hex().to_string();
        blobs.0.insert(key.clone(), bytes);
        entries.push((
            RelPath::new(rel.into_bytes()).unwrap(),
            ManifestEntry::Regular {
                mode: 0o100644,
                planes: RegularPlanes {
                    semantic: None,
                    callgraph: Some(key),
                },
                resolution_input: path.file_name().and_then(|name| name.to_str())
                    == Some("Cargo.toml"),
            },
        ));
    }
    (Manifest::new(entries).unwrap(), blobs)
}

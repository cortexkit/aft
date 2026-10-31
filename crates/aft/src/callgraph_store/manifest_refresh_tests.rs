//! Manifest refresh work counters and cold-build differentials.

use super::*;
use std::fs;

#[test]
#[ignore = "requires a frozen real workspace in AFT_MANIFEST_CORPUS"]
fn bench_cargo_manifest_refresh_real_workspace() {
    let root = PathBuf::from(std::env::var_os("AFT_MANIFEST_CORPUS").unwrap());
    let root = root.canonicalize().unwrap();
    let mut files: Vec<_> = callgraph::walk_project_files(&root)
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    let manifest = root.join("crates/aft-tokenizer/Cargo.toml");
    files.push(manifest.clone());
    files.sort();
    let original = fs::read_to_string(&manifest).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let store = CallGraphStore::open(temp.path().join("warm"), root.clone()).unwrap();
    let cold = store.cold_build(&files).unwrap();
    eprintln!(
        "MANIFEST_CORPUS files={} refs={} cold_ms={}",
        cold.files, cold.refs, cold.elapsed_ms
    );
    for (case, text) in [
        (
            "no_semantic_change",
            format!("{original}\n# manifest refresh measurement\n"),
        ),
        (
            "added_dependency",
            format!("{original}\n[dev-dependencies.manifest-refresh-probe]\nversion = \"1\"\n"),
        ),
        (
            "renamed_crate",
            original.replace("name = \"aft-tokenizer\"", "name = \"renamed-tokenizer\""),
        ),
    ] {
        fs::write(&manifest, text).unwrap();
        reset_workspace_crate_prefix_build_count(&root);
        let session = rust_module_work::Session::start(false);
        let (stats, profile) = store
            .refresh_files_profiled(std::slice::from_ref(&manifest))
            .unwrap();
        let work = session.finish();
        eprintln!("MANIFEST_CASE {case} files={} refs={} selected_refs={} prefix_builds={} profile={} parent_scans={} parent_file_visits={} caller_walks={} parent_ms={} caller_ms={}", work.resolved_files.len(), work.resolved_refs, stats.dependency_selected_refs, workspace_crate_prefix_build_count(&root), profile.report(), work.parent_scans, work.parent_file_visits, work.caller_walks, work.parent_time.as_millis(), work.caller_time.as_millis());
        let rebuilt = CallGraphStore::open(temp.path().join(case), root.clone()).unwrap();
        rebuilt.cold_build(&files).unwrap();
        incremental_parity::compare(&store, &rebuilt).unwrap();
        eprintln!("MANIFEST_PARITY {case} every_table=identical");
    }
    fs::write(manifest, original).unwrap();
}

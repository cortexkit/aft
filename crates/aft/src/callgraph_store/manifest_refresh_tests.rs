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

struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
    files: Vec<PathBuf>,
    store: CallGraphStore,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let mut files = Vec::new();
        for (path, source) in [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"dep\", \"facade\", \"facade2\", \"app\", \"other\"]\n",
            ),
            (
                "dep/Cargo.toml",
                "[package]\nname = \"dep-crate\"\nversion = \"0.1.0\"\n",
            ),
            ("facade/Cargo.toml", "[package]\nname = \"facade\"\n"),
            ("facade2/Cargo.toml", "[package]\nname = \"facade2\"\n"),
            ("app/Cargo.toml", "[package]\nname = \"app\"\n"),
            ("other/Cargo.toml", "[package]\nname = \"other\"\n"),
            (
                "dep/src/lib.rs",
                "mod root; pub fn target() { helper(); } fn helper() {}\n",
            ),
            (
                "dep/src/root.rs",
                "pub mod leaf; pub fn run() { leaf::target(); }\n",
            ),
            ("dep/src/root/leaf.rs", "pub fn target() {}\n"),
            ("dep/src/leaf.rs", "pub fn target() {}\n"),
            (
                "facade/src/lib.rs",
                "pub use dep_crate::target as forwarded;\n",
            ),
            (
                "facade2/src/lib.rs",
                "pub use facade::forwarded as rerouted;\n",
            ),
            (
                "app/src/lib.rs",
                "mod direct; mod alias; mod glob; mod indirect; pub fn local() {}\n",
            ),
            (
                "app/src/direct.rs",
                "pub fn direct() { dep_crate::target(); fresh_crate::target(); }\n",
            ),
            (
                "app/src/alias.rs",
                "use dep_crate as alias; pub fn aliased() { alias::target(); }\n",
            ),
            (
                "app/src/glob.rs",
                "use dep_crate::*; pub fn globbed() { target(); }\n",
            ),
            (
                "app/src/indirect.rs",
                "use facade2::rerouted as hop; pub fn indirect() { hop(); }\n",
            ),
            (
                "other/src/lib.rs",
                "pub fn untouched() { local(); } fn local() {}\n",
            ),
        ] {
            let file = root.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, source).unwrap();
            files.push(file);
        }
        let store = CallGraphStore::open(temp.path().join("incremental"), root.clone()).unwrap();
        store.cold_build(&files).unwrap();
        Self {
            temp,
            root,
            files,
            store,
        }
    }

    fn refresh(&self, path: &str, source: &str) -> rust_module_work::Work {
        let manifest = self.root.join(path);
        fs::write(&manifest, source).unwrap();
        reset_workspace_crate_prefix_build_count(&self.root);
        let session = rust_module_work::Session::start(false);
        self.store.refresh_files(&[manifest]).unwrap();
        let work = session.finish();
        assert_eq!(workspace_crate_prefix_build_count(&self.root), 1);
        let cold = CallGraphStore::open(self.temp.path().join("cold"), self.root.clone()).unwrap();
        cold.cold_build(&self.files).unwrap();
        incremental_parity::compare(&self.store, &cold).unwrap();
        for (file, symbol) in [
            ("app/src/direct.rs", "direct"),
            ("app/src/indirect.rs", "indirect"),
        ] {
            assert_eq!(
                serde_json::to_value(self.store.call_tree(Path::new(file), symbol, 3).unwrap())
                    .unwrap(),
                serde_json::to_value(cold.call_tree(Path::new(file), symbol, 3).unwrap()).unwrap(),
            );
        }
        work
    }
}

#[test]
fn cargo_manifest_noop_resolves_zero_references() {
    let mut fixture = Fixture::new();
    // Reopening must use committed inputs, not a process-local pre-edit cache.
    let directory = fixture.store.sqlite_path().parent().unwrap().to_path_buf();
    drop(fixture.store);
    fixture.store = CallGraphStore::open(directory, fixture.root.clone()).unwrap();
    let work = fixture.refresh("dep/Cargo.toml", "[package]\nname = \"dep-crate\"\nversion = \"0.1.0\"\n# a branch switch changed only this comment\n");
    assert_eq!(work.resolved_refs, 0);
    assert!(work.resolved_files.is_empty());
}

#[test]
fn cargo_manifest_added_dependency_resolves_zero_references() {
    let fixture = Fixture::new();
    let work = fixture.refresh("dep/Cargo.toml", "[package]\nname = \"dep-crate\"\nversion = \"0.1.0\"\n[dependencies]\nother = { path = \"../other\" }\n");
    assert_eq!(work.resolved_refs, 0);
    assert!(work.resolved_files.is_empty());
}

#[test]
fn cargo_manifest_rename_resolves_only_affected_files() {
    let fixture = Fixture::new();
    let before = fixture
        .store
        .call_tree(Path::new("app/src/direct.rs"), "direct", 1)
        .unwrap();
    assert!(before.children[0].resolved);
    assert!(!before.children[1].resolved);
    let work = fixture.refresh(
        "dep/Cargo.toml",
        "[package]\nname = \"fresh-crate\"\nversion = \"0.1.0\"\n",
    );
    assert!(work.resolved_refs > 0);
    assert_eq!(
        work.resolved_files,
        BTreeSet::from([
            "app/src/direct.rs".to_string(),
            "app/src/alias.rs".to_string(),
            "app/src/glob.rs".to_string(),
            "facade/src/lib.rs".to_string(),
        ])
    );
    let after = fixture
        .store
        .call_tree(Path::new("app/src/direct.rs"), "direct", 1)
        .unwrap();
    assert!(!after.children[0].resolved);
    assert!(after.children[1].resolved);
}

#[test]
fn cargo_manifest_lib_name_selects_new_name_without_refreshing_package_importers() {
    let fixture = Fixture::new();
    let work = fixture.refresh(
        "dep/Cargo.toml",
        "[package]\nname = \"dep-crate\"\nversion = \"0.1.0\"\n[lib]\nname = \"fresh_crate\"\n",
    );
    assert_eq!(
        work.resolved_files,
        BTreeSet::from(["app/src/direct.rs".to_string()])
    );
    let tree = fixture
        .store
        .call_tree(Path::new("app/src/direct.rs"), "direct", 1)
        .unwrap();
    assert!(tree.children[0].resolved);
    assert!(tree.children[1].resolved);
}

#[test]
fn cargo_manifest_missing_baseline_refreshes_conservatively() {
    let fixture = Fixture::new();
    fixture
        .store
        .conn
        .lock()
        .unwrap()
        .execute("DELETE FROM meta WHERE k = 'rust_manifest_inputs'", [])
        .unwrap();
    let work = fixture.refresh(
        "dep/Cargo.toml",
        "[package]\nname = \"dep-crate\"\nversion = \"0.1.0\"\n# pre-hint store\n",
    );
    assert!(work.resolved_refs > 0);
    assert!(work.resolved_files.contains("other/src/lib.rs"));
}

#[test]
fn cargo_manifest_workspace_members_do_not_change_stored_resolver_bindings() {
    let fixture = Fixture::new();
    let work = fixture.refresh("Cargo.toml", "[workspace]\nmembers = [\"dep\", \"app\"]\n");
    assert_eq!(work.resolved_refs, 0);
}

#[test]
fn cargo_manifest_target_root_refreshes_module_structure_without_sibling_crates() {
    let fixture = Fixture::new();
    let before = fixture
        .store
        .call_tree(Path::new("dep/src/root.rs"), "run", 1)
        .unwrap();
    assert_eq!(before.children[0].file, "dep/src/root/leaf.rs");
    let work = fixture.refresh(
        "dep/Cargo.toml",
        "[package]\nname = \"dep-crate\"\nversion = \"0.1.0\"\n[lib]\npath = \"src/root.rs\"\n",
    );
    let after = fixture
        .store
        .call_tree(Path::new("dep/src/root.rs"), "run", 1)
        .unwrap();
    assert_eq!(after.children[0].file, "dep/src/leaf.rs");
    assert!(work.resolved_files.contains("dep/src/root.rs"));
    assert!(!work.resolved_files.contains("other/src/lib.rs"));
}

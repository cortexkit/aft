use std::fs;
use std::time::Duration;

use aft::parser::{FileParser, SymbolCache};
use aft::symbol_cache_disk;

fn cache_file(storage: &tempfile::TempDir, project_key: &str) -> std::path::PathBuf {
    storage
        .path()
        .join("symbols")
        .join(project_key)
        .join("symbols.bin")
}

fn write_source(project: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    let path = project.path().join("src/lib.rs");
    fs::create_dir_all(path.parent().expect("source parent")).expect("create source dir");
    fs::write(&path, body).expect("write source file");
    path
}

fn rewrite_with_advanced_mtime(path: &std::path::Path, body: &str) {
    let modified = fs::metadata(path)
        .expect("stat source")
        .modified()
        .expect("source mtime");
    let advanced = modified
        .checked_add(Duration::from_secs(2))
        .expect("advanced mtime");
    fs::write(path, body).expect("rewrite source");
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(advanced))
        .expect("set advanced source mtime");
}

fn build_symbol_cache(project: &tempfile::TempDir, source: &std::path::Path) -> SymbolCache {
    let mut parser = FileParser::new();
    let symbols = parser
        .extract_symbols(source)
        .expect("extract source symbols");
    assert!(!symbols.is_empty(), "test source should produce symbols");

    let shared = parser.symbol_cache();
    let mut cache = shared.write().expect("write symbol cache").clone();
    cache.set_project_root(project.path().to_path_buf());
    cache
}

#[test]
fn cold_start_populates_and_persists_symbol_cache() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let source = write_source(&project, "pub fn cold_start() -> bool { true }\n");
    let cache = build_symbol_cache(&project, &source);

    symbol_cache_disk::write_to_disk(&cache, storage.path(), "cold-project")
        .expect("write symbol cache");

    assert!(cache_file(&storage, "cold-project").exists());
}

#[test]
fn warm_restart_loads_entries_into_fresh_symbol_cache() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let source = write_source(&project, "pub fn warm_restart() -> bool { true }\n");
    let cache = build_symbol_cache(&project, &source);
    let metadata = fs::metadata(&source).expect("stat source");
    let mtime = metadata.modified().expect("source mtime");
    let original = cache.get(&source, mtime).expect("cached original symbols");

    symbol_cache_disk::write_to_disk(&cache, storage.path(), "warm-project")
        .expect("write symbol cache");

    let mut fresh = SymbolCache::new();
    let loaded = fresh.load_from_disk(storage.path(), "warm-project", project.path());
    let restored = fresh.get(&source, mtime).expect("restored symbols");

    assert_eq!(loaded, 1);
    assert_eq!(restored.len(), original.len());
    assert_eq!(restored[0].name, original[0].name);
}

#[test]
fn mtime_invalidation_drops_changed_entry() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let source = write_source(&project, "pub fn mtime_change() -> bool { true }\n");
    let cache = build_symbol_cache(&project, &source);

    symbol_cache_disk::write_to_disk(&cache, storage.path(), "mtime-project")
        .expect("write symbol cache");
    rewrite_with_advanced_mtime(&source, "pub fn mtime_change() -> bool { true }\n");

    let mut fresh = SymbolCache::new();
    let loaded = fresh.load_from_disk(storage.path(), "mtime-project", project.path());

    assert_eq!(loaded, 1);
    assert_eq!(fresh.len(), 1);
}

#[test]
fn size_invalidation_drops_changed_entry() {
    let project = tempfile::tempdir().expect("create project dir");
    let storage = tempfile::tempdir().expect("create storage dir");
    let source = write_source(&project, "pub fn size_change() -> bool { true }\n");
    let cache = build_symbol_cache(&project, &source);

    symbol_cache_disk::write_to_disk(&cache, storage.path(), "size-project")
        .expect("write symbol cache");
    fs::write(
        &source,
        "pub fn size_change() -> bool { true }\npub fn added() {}\n",
    )
    .expect("rewrite source with different size");

    let mut fresh = SymbolCache::new();
    let loaded = fresh.load_from_disk(storage.path(), "size-project", project.path());

    assert_eq!(loaded, 0);
    assert_eq!(fresh.len(), 0);
}

#[test]
fn corrupt_file_recovery_returns_none_without_panicking() {
    let storage = tempfile::tempdir().expect("create storage dir");
    let path = cache_file(&storage, "corrupt-project");
    fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
    fs::write(&path, b"garbage").expect("write corrupt cache");

    assert!(symbol_cache_disk::read_from_disk(storage.path(), "corrupt-project").is_none());
}

#[test]
fn wrong_version_returns_none_without_panicking() {
    let storage = tempfile::tempdir().expect("create storage dir");
    let path = cache_file(&storage, "version-project");
    fs::create_dir_all(path.parent().expect("cache parent")).expect("create cache dir");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"AFTSYM1\0");
    bytes.extend_from_slice(&999u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    fs::write(&path, bytes).expect("write wrong-version cache");

    assert!(symbol_cache_disk::read_from_disk(storage.path(), "version-project").is_none());
}

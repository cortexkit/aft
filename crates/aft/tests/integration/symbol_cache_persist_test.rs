use std::fs;
use std::time::Duration;

use aft::parser::{FileParser, SymbolCache};

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

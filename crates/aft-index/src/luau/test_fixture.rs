
#[doc(hidden)]
pub const FIXTURE: &str = include_str!("fixtures/symbols.luau");

const WASM: &[u8] = include_bytes!(
    "fixtures/56b2a562244e6f35f7b320bce59099126185389df1277a93e450ba4156c0e039.wasm"
);

#[doc(hidden)]
pub fn ready() {
    static DIRECTORY: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIRECTORY.get_or_init(|| {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("grammars");
        crate::private_storage::create_dir_all(&root).unwrap();
        let manifest = crate::grammar::manifest();
        crate::private_storage::write(root.join(format!("{}.wasm", manifest.sha256)), WASM)
            .unwrap();
        let mut config = crate::config::Config::default();
        config.storage_dir = Some(directory.path().to_path_buf());
        crate::grammar::configure(&config).stored().unwrap();
        directory
    });
}

#[doc(hidden)]
pub fn tree(source: &str) -> tree_sitter::Tree {
    ready();
    let tree =
        crate::parser::parse_luau_source(std::path::Path::new("fixture.luau"), source).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    tree
}

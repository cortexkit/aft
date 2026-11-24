    pub(crate) const FIXTURE: &str = include_str!("../tests/fixtures/luau/symbols.luau");

    pub(crate) fn ready() {
        static DIRECTORY: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        DIRECTORY.get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("grammars");
            crate::private_storage::create_dir_all(&root).unwrap();
            let manifest = crate::grammar::manifest();
            let fixture = crate::test_support::manifest_dir()
                .join("tests/fixtures/grammars")
                .join(format!("{}.wasm", manifest.sha256));
            crate::private_storage::write(
                root.join(format!("{}.wasm", manifest.sha256)),
                std::fs::read(fixture).unwrap(),
            )
            .unwrap();
            let mut config = crate::config::Config::default();
            config.storage_dir = Some(directory.path().to_path_buf());
            crate::grammar::configure(&config).stored().unwrap();
            directory
        });
    }

    pub(crate) fn tree(source: &str) -> tree_sitter::Tree {
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

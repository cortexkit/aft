use std::path::{Path, PathBuf};

fn compile_time_checkout_path() -> regex::Regex {
    regex::Regex::new(r#"\b(?:env|option_env)\s*!\s*\(\s*"CARGO_MANIFEST_DIR""#).unwrap()
}

fn rust_sources(directory: &Path, sources: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            rust_sources(&path, sources);
        } else if kind.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
}

#[test]
fn rust_crates_do_not_embed_the_checkout_path() {
    let crates = crate::test_helpers::cargo_manifest_dir()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut sources = Vec::new();
    rust_sources(&crates, &mut sources);
    assert!(sources.contains(&crates.join("aft/src/test_support.rs")));
    sources.sort();

    let forbidden = compile_time_checkout_path();
    let mut violations = Vec::new();
    for source in sources {
        let contents = std::fs::read_to_string(&source).unwrap();
        for found in forbidden.find_iter(&contents) {
            let line = contents[..found.start()]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            violations.push(format!(
                "{}:{line}",
                source.strip_prefix(&crates).unwrap().display()
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "read CARGO_MANIFEST_DIR at runtime instead of embedding the checkout path:\n{}",
        violations.join("\n")
    );
}

#[test]
fn checkout_path_guard_rejects_compile_time_macros() {
    let forbidden = compile_time_checkout_path();
    for name in ["env", "option_env"] {
        for spacing in ["", " \n "] {
            let source = format!("{name}{spacing}!{spacing}({spacing}\"CARGO_MANIFEST_DIR\")");
            assert!(forbidden.is_match(&source), "missed {source}");
        }
    }
}

#[test]
fn checkout_path_guard_allows_runtime_reads_and_relative_includes() {
    let source = r#"
        std::env::var_os("CARGO_MANIFEST_DIR");
        include_str!("../fixtures/source.rs");
        include_bytes!("fixtures/data.bin");
        env!("CARGO_PKG_VERSION");
    "#;
    assert!(!compile_time_checkout_path().is_match(source));
}

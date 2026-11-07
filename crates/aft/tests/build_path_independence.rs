use std::collections::{BTreeSet, VecDeque};
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

fn manifest(directory: &Path) -> toml::Value {
    let path = directory.join("Cargo.toml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .parse()
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn build_script_violations(workspace: &Path, package: &Path) -> Vec<String> {
    let root = manifest(workspace);
    let mut members = BTreeSet::new();
    for member in root["workspace"]["members"].as_array().unwrap() {
        let pattern = workspace.join(member.as_str().unwrap());
        for directory in glob::glob(pattern.to_str().unwrap()).unwrap() {
            members.insert(directory.unwrap().canonicalize().unwrap());
        }
    }

    let mut pending = VecDeque::from([package.canonicalize().unwrap()]);
    let mut visited = BTreeSet::new();
    let mut violations = Vec::new();
    while let Some(directory) = pending.pop_front() {
        if !visited.insert(directory.clone()) {
            continue;
        }
        let package = manifest(&directory);
        let name = package["package"]["name"].as_str().unwrap();
        if directory.join("build.rs").exists() {
            violations.push(format!("{name}: build.rs"));
        }
        if package["package"].get("build").is_some() {
            violations.push(format!("{name}: package.build"));
        }

        let mut tables = vec![package.get("dependencies")];
        if let Some(targets) = package.get("target").and_then(toml::Value::as_table) {
            tables.extend(targets.values().map(|target| target.get("dependencies")));
        }
        for dependencies in tables
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_table)
        {
            for (alias, dependency) in dependencies {
                let inherited =
                    dependency.get("workspace").and_then(toml::Value::as_bool) == Some(true);
                let (base, dependency) = if inherited {
                    (
                        workspace,
                        &root["workspace"]["dependencies"][alias.as_str()],
                    )
                } else {
                    (directory.as_path(), dependency)
                };
                let Some(path) = dependency.get("path").and_then(toml::Value::as_str) else {
                    continue;
                };
                let dependency = base.join(path).canonicalize().unwrap();
                if members.contains(&dependency) {
                    pending.push_back(dependency);
                }
            }
        }
    }
    violations
}

#[test]
fn normal_workspace_dependencies_do_not_have_build_scripts() {
    let package = crate::test_helpers::manifest_dir();
    let workspace = package.parent().unwrap().parent().unwrap();
    let violations = build_script_violations(workspace, package);
    assert!(
        violations.is_empty(),
        "shared build folders key workspace crates by content; a build script disables that:\n{}",
        violations.join("\n")
    );
}

#[test]
fn build_script_guard_follows_normal_target_and_inherited_dependencies_only() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        r#"
        [workspace]
        members = ["app", "core", "fixture"]
        [workspace.dependencies]
        renamed = { package = "core", path = "core" }
    "#,
    )
    .unwrap();
    for name in ["app", "core", "fixture"] {
        let directory = root.path().join(name);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            directory.join("Cargo.toml"),
            format!("[package]\nname = {name:?}\n"),
        )
        .unwrap();
    }
    std::fs::write(root.path().join("fixture/build.rs"), "fn main() {}").unwrap();
    std::fs::write(
        root.path().join("app/Cargo.toml"),
        r#"
        [package]
        name = "app"
        [target.'cfg(windows)'.dependencies]
        renamed.workspace = true
        [dev-dependencies]
        fixture = { path = "../fixture" }
    "#,
    )
    .unwrap();
    let app = root.path().join("app");
    assert!(build_script_violations(root.path(), &app).is_empty());
    std::fs::write(root.path().join("core/build.rs"), "fn main() {}").unwrap();
    assert_eq!(
        build_script_violations(root.path(), &app),
        ["core: build.rs"]
    );
    std::fs::write(
        root.path().join("core/Cargo.toml"),
        "[package]\nname = 'core'\nbuild = false\n",
    )
    .unwrap();
    assert_eq!(
        build_script_violations(root.path(), &app),
        ["core: build.rs", "core: package.build"]
    );
    std::fs::write(
        root.path().join("core/Cargo.toml"),
        "[package]\nname = 'core'\n[dependencies]\nfixture = {path = '../fixture'}\n",
    )
    .unwrap();
    assert_eq!(
        build_script_violations(root.path(), &app),
        ["core: build.rs", "fixture: build.rs"]
    );
}

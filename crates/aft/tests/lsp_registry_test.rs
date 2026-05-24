use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use aft::config::{Config, UserServerDef};
use aft::lsp::registry::{servers_for_file, ServerKind};
use aft::lsp::roots::find_workspace_root;
use tempfile::tempdir;

#[test]
fn test_deepest_root_wins_in_monorepo() {
    let temp_dir = tempdir().unwrap();
    let monorepo_root = temp_dir.path().join("monorepo");
    let crate_root = monorepo_root.join("crates").join("my-crate");
    let src_dir = crate_root.join("src");
    let main_rs = src_dir.join("main.rs");

    fs::create_dir_all(&src_dir).unwrap();
    fs::write(monorepo_root.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(
        crate_root.join("Cargo.toml"),
        "[package]\nname = \"my-crate\"\n",
    )
    .unwrap();
    fs::write(&main_rs, "fn main() {}\n").unwrap();

    let expected_root = fs::canonicalize(&crate_root).unwrap();
    assert_eq!(
        find_workspace_root(&main_rs, &["Cargo.toml"]),
        Some(expected_root)
    );
}

#[test]
fn test_typescript_root_with_tsconfig() {
    let temp_dir = tempdir().unwrap();
    let root = temp_dir.path().join("web");
    let src_dir = root.join("src");
    let file = src_dir.join("index.ts");

    fs::create_dir_all(&src_dir).unwrap();
    fs::write(root.join("tsconfig.json"), "{}\n").unwrap();
    fs::write(&file, "export const value = 1;\n").unwrap();

    let expected_root = fs::canonicalize(&root).unwrap();
    assert_eq!(
        find_workspace_root(&file, &["tsconfig.json", "package.json"]),
        Some(expected_root)
    );
}

#[test]
fn test_registry_and_root_combined() {
    let temp_dir = tempdir().unwrap();
    let root = temp_dir.path().join("crate");
    let src_dir = root.join("src");
    let file = src_dir.join("main.rs");

    fs::create_dir_all(&src_dir).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    fs::write(&file, "fn main() {}\n").unwrap();

    let config = Config::default();
    let servers = servers_for_file(&file, &config);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].kind, ServerKind::Rust);
    let expected_root = fs::canonicalize(&root).unwrap();
    assert_eq!(
        find_workspace_root(&file, &servers[0].root_markers),
        Some(expected_root)
    );
}

#[test]
fn test_bash_yaml_and_ty_registry_entries() {
    let default_config = Config::default();
    assert_eq!(
        servers_for_file(std::path::Path::new("/tmp/script.bash"), &default_config)
            .into_iter()
            .map(|server| server.kind)
            .collect::<Vec<_>>(),
        vec![ServerKind::Bash]
    );
    assert_eq!(
        servers_for_file(std::path::Path::new("/tmp/config.yml"), &default_config)
            .into_iter()
            .map(|server| server.kind)
            .collect::<Vec<_>>(),
        vec![ServerKind::Yaml]
    );

    let ty_config = Config {
        experimental_lsp_ty: true,
        ..Config::default()
    };
    assert_eq!(
        servers_for_file(std::path::Path::new("/tmp/main.py"), &ty_config)
            .into_iter()
            .map(|server| server.kind)
            .collect::<Vec<_>>(),
        vec![ServerKind::Python, ServerKind::Ty]
    );
}

#[test]
fn test_ty_hidden_when_flag_off_but_visible_when_on() {
    let default_kinds = servers_for_file(std::path::Path::new("/tmp/main.py"), &Config::default())
        .into_iter()
        .map(|server| server.kind)
        .collect::<Vec<_>>();
    assert_eq!(default_kinds, vec![ServerKind::Python]);

    let config = Config {
        experimental_lsp_ty: true,
        ..Config::default()
    };
    let enabled_kinds = servers_for_file(std::path::Path::new("/tmp/main.py"), &config)
        .into_iter()
        .map(|server| server.kind)
        .collect::<Vec<_>>();
    assert_eq!(enabled_kinds, vec![ServerKind::Python, ServerKind::Ty]);
}

#[test]
fn test_custom_server_registers_correctly() {
    // Use an extension no built-in claims so the custom server is the only match.
    let config = Config {
        lsp_servers: vec![UserServerDef {
            id: "my-custom".to_string(),
            extensions: vec!["xyzcustom".to_string()],
            binary: "my-custom-bin".to_string(),
            args: Vec::new(),
            root_markers: vec!["custom.toml".to_string()],
            env: HashMap::new(),
            initialization_options: None,
            disabled: false,
        }],
        ..Config::default()
    };

    let servers = servers_for_file(std::path::Path::new("/tmp/main.xyzcustom"), &config);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].kind, ServerKind::Custom(Arc::from("my-custom")));
    assert_eq!(servers[0].binary, "my-custom-bin");
}

#[test]
fn test_custom_server_coexists_with_builtin_for_same_extension() {
    // After v0.17.0, AFT registers `tinymist` as a built-in. A user-defined
    // server with the same extension should coexist (not replace) the built-in.
    let config = Config {
        lsp_servers: vec![UserServerDef {
            id: "tinymist-fork".to_string(),
            extensions: vec!["typ".to_string()],
            binary: "tinymist-fork".to_string(),
            args: Vec::new(),
            root_markers: vec!["typst.toml".to_string()],
            env: HashMap::new(),
            initialization_options: None,
            disabled: false,
        }],
        ..Config::default()
    };

    let kinds: Vec<ServerKind> = servers_for_file(std::path::Path::new("/tmp/main.typ"), &config)
        .into_iter()
        .map(|s| s.kind)
        .collect();
    assert!(kinds.contains(&ServerKind::Tinymist));
    assert!(kinds.contains(&ServerKind::Custom(Arc::from("tinymist-fork"))));
}

#[test]
fn test_disabled_lsp_filters_builtins_and_custom_servers() {
    let mut config = Config {
        lsp_servers: vec![UserServerDef {
            id: "Tinymist".to_string(),
            extensions: vec!["typ".to_string()],
            binary: "tinymist".to_string(),
            args: Vec::new(),
            root_markers: Vec::new(),
            env: HashMap::new(),
            initialization_options: None,
            disabled: false,
        }],
        ..Config::default()
    };
    config.disabled_lsp.insert("rust".to_string());
    config.disabled_lsp.insert("tinymist".to_string());

    assert!(servers_for_file(std::path::Path::new("/tmp/main.rs"), &config).is_empty());
    assert!(servers_for_file(std::path::Path::new("/tmp/main.typ"), &config).is_empty());
}

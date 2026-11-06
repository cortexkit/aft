use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{is_os_metadata_file_name, is_os_metadata_path};
use crate::context::AppContext;
use crate::parser::TreeSitterProvider;
use crate::protocol::RawRequest;
use crate::search_index::SearchIndex;

/// The metadata files an "empty" macOS or Windows folder typically holds.
const METADATA_ONLY: [&str; 3] = [".localized", ".DS_Store", "Thumbs.db"];

fn fixture(files: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("fixture dir");
    let root = std::fs::canonicalize(dir.path()).expect("canonical root");
    for name in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
        let content = if name.ends_with(".rs") {
            "pub fn indexed_function() -> u32 {\n    42\n}\n"
        } else {
            ""
        };
        std::fs::write(&path, content).expect("fixture file");
    }
    (dir, root)
}

fn context(root: &Path, trigram: bool) -> AppContext {
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.to_path_buf()),
            ..crate::config::Config::default()
        },
    );
    ctx.update_config(|config| {
        config.indexes.trigram = trigram;
        config.indexes.semantic = false;
        config.indexes.callgraph = false;
    });
    ctx
}

fn request(command: &str, params: Value) -> RawRequest {
    RawRequest {
        id: format!("os-metadata-{command}"),
        command: command.to_string(),
        lsp_hints: None,
        session_id: None,
        params,
    }
}

/// Files returned by `glob **/*`, answered from a ready search index when
/// `use_index` is set and from the filesystem walk otherwise.
fn glob_all(root: &Path, use_index: bool) -> Vec<String> {
    let ctx = context(root, use_index);
    if use_index {
        crate::data_layer::surfaces::serve_text_index_for_test(&ctx);
    }
    let response =
        crate::commands::glob::handle_glob(&request("glob", json!({ "pattern": "**/*" })), &ctx);
    assert!(response.success, "glob failed: {:?}", response.data);
    response.data["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| {
            Path::new(file.as_str().expect("file path"))
                .strip_prefix(root)
                .expect("file under root")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

#[test]
fn matches_exact_metadata_names_only() {
    for name in [
        ".DS_Store",
        ".localized",
        "Icon\r",
        "Thumbs.db",
        "ehthumbs.db",
        "desktop.ini",
        "._main.rs",
        "._",
    ] {
        assert!(is_os_metadata_file_name(OsStr::new(name)), "{name:?}");
    }
    for name in [
        "Icon",
        "Icon.png",
        "thumbs.db",
        "Desktop.ini",
        ".DS_Store.bak",
        "my.DS_Store",
        "_main.rs",
        ".gitignore",
        "main.rs",
    ] {
        assert!(!is_os_metadata_file_name(OsStr::new(name)), "{name:?}");
    }
    assert!(is_os_metadata_path(Path::new("/proj/deep/dir/.DS_Store")));
    // Only the file name counts: a directory that merely contains one of these
    // names in its own path is not metadata.
    assert!(!is_os_metadata_path(Path::new("/proj/._odd/main.rs")));
}

#[test]
fn nested_metadata_files_are_skipped_at_any_depth() {
    // Windows forbids `\r` in file names, so the macOS folder-icon file can
    // only be created on Unix; the name check itself is covered on every
    // platform by the unit test above.
    #[cfg(unix)]
    let files: &[&str] = &["a/b/.DS_Store", "a/desktop.ini", "a/b/c/Icon\r"];
    #[cfg(not(unix))]
    let files: &[&str] = &["a/b/.DS_Store", "a/desktop.ini", "a/b/c/._notes.rs"];
    let (_dir, root) = fixture(files);

    assert_eq!(SearchIndex::build(&root).file_count(), 0);
    assert_eq!(glob_all(&root, false), Vec::<String>::new());
}

#[test]
fn explicit_read_of_a_metadata_file_still_works() {
    let (_dir, root) = fixture(&[]);
    let path = root.join(".DS_Store");
    std::fs::write(&path, "finder view settings\n").expect("write .DS_Store");
    let ctx = context(&root, false);

    let response = crate::commands::read::handle_read(
        &request("read", json!({ "file": path.display().to_string() })),
        &ctx,
    );

    assert!(response.success, "read failed: {:?}", response.data);
    let content = response.data["content"].as_str().expect("content");
    assert!(content.contains("finder view settings"), "{content}");
}

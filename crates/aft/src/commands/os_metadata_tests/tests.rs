use std::path::Path;

use serde_json::json;

use crate::context::AppContext;
use crate::parser::TreeSitterProvider;

use crate::search_index::SearchIndex;
#[allow(unused_imports)]
pub(crate) use aft_base::os_metadata::tests::*;

fn context(root: &Path, trigram: bool) -> AppContext {
    let ctx = crate::test_context(
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

/// Files returned by `glob **/*`, answered from a ready search index when
/// `use_index` is set and from the filesystem walk otherwise.
fn glob_all(root: &Path, use_index: bool) -> Vec<String> {
    let ctx = context(root, use_index);
    if use_index {
        aft_engine::test_bridge::display_data::surfaces::serve_text_index_for_test(&ctx);
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

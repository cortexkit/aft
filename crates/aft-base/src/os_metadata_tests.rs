#[cfg(test)]
use std::ffi::OsStr;

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

#[cfg(test)]
use super::is_os_metadata_file_name;
#[cfg(test)]
use super::is_os_metadata_path;

use crate::protocol::RawRequest;

pub fn fixture(files: &[&str]) -> (tempfile::TempDir, PathBuf) {
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

pub fn request(command: &str, params: Value) -> RawRequest {
    RawRequest {
        id: format!("os-metadata-{command}"),
        command: command.to_string(),
        lsp_hints: None,
        session_id: None,
        params,
    }
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

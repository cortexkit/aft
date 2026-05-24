use std::path::{Path, PathBuf};

use crate::lsp::registry::ServerKind;

/// Find the workspace root for a file given root marker filenames.
/// Walks up from the file's parent directory looking for any of the markers.
/// Returns the deepest directory containing a marker (closest to the file).
/// If no marker is found, returns None.
pub fn find_workspace_root<S>(file_path: &Path, markers: &[S]) -> Option<PathBuf>
where
    S: AsRef<str>,
{
    let resolved_path = match std::fs::canonicalize(file_path) {
        Ok(path) => path,
        Err(_) => file_path.to_path_buf(),
    };

    let start_dir = if resolved_path.is_dir() {
        resolved_path
    } else {
        resolved_path.parent()?.to_path_buf()
    };

    let mut current = Some(start_dir.as_path());
    while let Some(dir) = current {
        if markers
            .iter()
            .any(|marker| dir.join(marker.as_ref()).exists())
        {
            return Some(dir.to_path_buf());
        }

        current = dir.parent();
    }

    None
}

/// Composite key for caching server instances.
/// Each unique (ServerKind, workspace_root) pair gets its own server process.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerKey {
    pub kind: ServerKind,
    pub root: PathBuf,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::{find_workspace_root, ServerKey};
    use crate::lsp::registry::ServerKind;

    #[test]
    fn test_find_root_with_cargo_toml() {
        let temp_dir = tempdir().unwrap();
        let root = temp_dir.path().join("workspace");
        let src_dir = root.join("src");
        let file = src_dir.join("lib.rs");

        fs::create_dir_all(&src_dir).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(&file, "fn main() {}\n").unwrap();

        let expected_root = fs::canonicalize(&root).unwrap();
        assert_eq!(
            find_workspace_root(&file, &["Cargo.toml"]),
            Some(expected_root)
        );
    }

    #[test]
    fn test_find_root_nested() {
        let temp_dir = tempdir().unwrap();
        let repo_root = temp_dir.path().join("repo");
        let crate_root = repo_root.join("crates").join("foo");
        let src_dir = crate_root.join("src");
        let file = src_dir.join("lib.rs");

        fs::create_dir_all(&src_dir).unwrap();
        fs::write(repo_root.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(crate_root.join("Cargo.toml"), "[package]\nname = \"foo\"\n").unwrap();
        fs::write(&file, "fn main() {}\n").unwrap();

        let expected_root = fs::canonicalize(&crate_root).unwrap();
        assert_eq!(
            find_workspace_root(&file, &["Cargo.toml"]),
            Some(expected_root)
        );
    }

    #[test]
    fn test_find_root_none() {
        let temp_dir = tempdir().unwrap();
        let src_dir = temp_dir.path().join("src");
        let file = src_dir.join("main.rs");

        fs::create_dir_all(&src_dir).unwrap();
        fs::write(&file, "fn main() {}\n").unwrap();

        assert_eq!(find_workspace_root(&file, &["Cargo.toml"]), None);
    }

    #[test]
    fn test_find_root_multiple_markers() {
        let temp_dir = tempdir().unwrap();
        let root = temp_dir.path().join("web");
        let src_dir = root.join("src");
        let file = src_dir.join("index.ts");

        fs::create_dir_all(&src_dir).unwrap();
        fs::write(root.join("tsconfig.json"), "{}\n").unwrap();
        fs::create_dir(root.join("package.json")).unwrap();
        fs::write(&file, "export {};\n").unwrap();

        let expected_root = fs::canonicalize(&root).unwrap();
        assert_eq!(
            find_workspace_root(&file, &["tsconfig.json", "package.json"]),
            Some(expected_root)
        );
    }

    #[test]
    fn test_server_key_equality() {
        let root = PathBuf::from("/tmp/workspace");
        let same = ServerKey {
            kind: ServerKind::Rust,
            root: root.clone(),
        };
        let equal = ServerKey {
            kind: ServerKind::Rust,
            root,
        };
        let different = ServerKey {
            kind: ServerKind::Rust,
            root: PathBuf::from("/tmp/other"),
        };

        assert_eq!(same, equal);
        assert_ne!(same, different);
    }
}

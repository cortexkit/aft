use std::path::{Path, PathBuf};

use super::applicability_cache::observe_path;
use crate::lsp::registry::ServerKind;

/// Walk entries below a canonical root already have resolved parents: the
/// applicability walker never descends through directory symlinks. Other callers
/// must resolve arbitrary paths before comparing workspace boundaries.
#[derive(Clone, Copy)]
pub(crate) enum RootPathMode {
    Resolve,
    WalkEntry,
}

impl RootPathMode {
    pub(crate) fn resolve(self, path: &Path) -> PathBuf {
        match self {
            Self::Resolve => crate::path_identity::canonicalize_normalized(path),
            Self::WalkEntry => path.to_path_buf(),
        }
    }
}

pub fn find_workspace_root<S>(file_path: &Path, markers: &[S]) -> Option<PathBuf>
where
    S: AsRef<str>,
{
    find_workspace_root_within(file_path, markers, None)
}

/// Find a marker root without walking above an optional session project root.
pub fn find_workspace_root_within<S>(
    file_path: &Path,
    markers: &[S],
    project_root: Option<&Path>,
) -> Option<PathBuf>
where
    S: AsRef<str>,
{
    find_workspace_root_with_mode(file_path, markers, project_root, RootPathMode::Resolve)
}

pub(crate) fn find_workspace_root_with_mode<S: AsRef<str>>(
    file_path: &Path,
    markers: &[S],
    project_root: Option<&Path>,
    mode: RootPathMode,
) -> Option<PathBuf> {
    // Route canonicalization through `canonicalize_normalized` so the returned
    // root is never a Windows verbatim (`\\?\C:\...`) path. This root flows into
    // `LspClient::spawn` -> `Command::current_dir(&root)`, and `CreateProcess`
    // rejects extended-length verbatim paths as `lpCurrentDirectory` (documented
    // Win32 limitation: "The lpCurrentDirectory string ... must not be a \\?\
    // prefixed path"). Without this strip EVERY LSP spawn on Windows fails
    // with "The system cannot find the path specified" and aft_inspect reports
    // servers as not installed (#174). On Unix `canonicalize_normalized` is
    // identity-equivalent to `fs::canonicalize` followed by lexical `.`/`..`
    // collapse, so non-Windows behavior is unchanged.
    // `canonicalize_normalized` falls back to lexical `.`/`..` collapse when
    // `fs::canonicalize` fails (e.g. the file is gone), matching the prior
    // fallback to the raw `file_path` for paths without `.`/`..` components.
    let resolved_path = mode.resolve(file_path);

    let start_dir = if resolved_path.is_dir() {
        resolved_path
    } else {
        resolved_path.parent()?.to_path_buf()
    };

    let project_root = project_root.map(|path| mode.resolve(path));
    // Do not walk upward out of a mounted start directory. This keeps an explicit
    // file on that mount usable while preventing root discovery from crossing it;
    // recursive descendants avoid ReadDir::drop's ENXIO daemon-abort failure too.
    let device_boundary = crate::walk_boundary::DeviceBoundary::for_root(&start_dir).ok();
    if project_root
        .as_ref()
        .is_some_and(|boundary| !start_dir.starts_with(boundary))
    {
        return None;
    }

    let mut current = Some(start_dir.as_path());
    while let Some(dir) = current {
        observe_path(dir);
        if device_boundary
            .as_ref()
            .is_some_and(|boundary| !boundary.should_descend(dir).unwrap_or(false))
        {
            break;
        }
        if project_root
            .as_ref()
            .is_some_and(|boundary| !dir.starts_with(boundary))
        {
            break;
        }
        if markers.iter().any(|marker| {
            let path = dir.join(marker.as_ref());
            observe_path(&path);
            path.exists()
        }) {
            return Some(dir.to_path_buf());
        }

        if project_root.as_deref() == Some(dir) {
            break;
        }
        current = dir.parent();
    }

    None
}

/// Find the Cargo workspace that owns `file_path`.
///
/// Rust Analyzer loads the complete Cargo workspace supplied by the client. A
/// member crate's manifest is therefore not a suitable server root: opening a
/// second member would otherwise start another full-workspace analyzer. The
/// optional project root bounds the walk so an enclosing checkout cannot claim
/// a nested session as one of its workspaces.
pub fn find_rust_workspace_root(file_path: &Path, project_root: Option<&Path>) -> Option<PathBuf> {
    find_rust_workspace_root_with_mode(file_path, project_root, RootPathMode::Resolve)
}

pub(crate) fn find_rust_workspace_root_with_mode(
    file_path: &Path,
    project_root: Option<&Path>,
    mode: RootPathMode,
) -> Option<PathBuf> {
    let resolved_path = mode.resolve(file_path);
    let start_dir = if resolved_path.is_dir() {
        resolved_path
    } else {
        resolved_path.parent()?.to_path_buf()
    };
    let project_root = project_root.map(|path| mode.resolve(path));
    // See `find_workspace_root_within`: stay on the start filesystem while
    // discovering an LSP root, but permit the explicitly named mount itself.
    let device_boundary = crate::walk_boundary::DeviceBoundary::for_root(&start_dir).ok();

    if project_root
        .as_ref()
        .is_some_and(|boundary| !start_dir.starts_with(boundary))
    {
        return None;
    }

    let crate_root = nearest_cargo_manifest_dir(&start_dir, project_root.as_deref())?;
    let mut current = Some(crate_root.as_path());
    while let Some(dir) = current {
        observe_path(dir);
        if device_boundary
            .as_ref()
            .is_some_and(|boundary| !boundary.should_descend(dir).unwrap_or(false))
        {
            break;
        }
        if project_root
            .as_ref()
            .is_some_and(|boundary| !dir.starts_with(boundary))
        {
            break;
        }
        if cargo_workspace_contains_crate(dir, &crate_root) {
            return Some(mode.resolve(dir));
        }
        if project_root.as_deref() == Some(dir) {
            break;
        }
        current = dir.parent();
    }

    None
}

fn nearest_cargo_manifest_dir(start_dir: &Path, project_root: Option<&Path>) -> Option<PathBuf> {
    let device_boundary = crate::walk_boundary::DeviceBoundary::for_root(start_dir).ok();
    let mut current = Some(start_dir);
    while let Some(dir) = current {
        observe_path(dir);
        if device_boundary
            .as_ref()
            .is_some_and(|boundary| !boundary.should_descend(dir).unwrap_or(false))
        {
            break;
        }
        if project_root.is_some_and(|boundary| !dir.starts_with(boundary)) {
            break;
        }
        let manifest = dir.join("Cargo.toml");
        observe_path(&manifest);
        if manifest.is_file() {
            return Some(dir.to_path_buf());
        }
        if project_root == Some(dir) {
            break;
        }
        current = dir.parent();
    }
    None
}

fn cargo_workspace_contains_crate(workspace_root: &Path, crate_root: &Path) -> bool {
    let path = workspace_root.join("Cargo.toml");
    observe_path(&path);
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(manifest) = contents.parse::<toml::Value>() else {
        return false;
    };
    let Some(workspace) = manifest.get("workspace").and_then(toml::Value::as_table) else {
        return false;
    };

    if workspace_root == crate_root {
        return true;
    }

    let Ok(crate_relative_path) = crate_root.strip_prefix(workspace_root) else {
        return false;
    };
    // Cargo gives literal members precedence over excludes. This only models
    // declared members; implicit membership through path dependencies is not
    // inferred here.
    if workspace
        .get("members")
        .and_then(toml::Value::as_array)
        .is_some_and(|members| {
            members
                .iter()
                .filter_map(toml::Value::as_str)
                .any(|member| Path::new(member.trim()) == crate_relative_path)
        })
    {
        return true;
    }
    if workspace
        .get("exclude")
        .and_then(toml::Value::as_array)
        .is_some_and(|patterns| {
            patterns
                .iter()
                .filter_map(toml::Value::as_str)
                .any(|pattern| crate_relative_path.starts_with(pattern.trim()))
        })
    {
        return false;
    }

    match workspace.get("members").and_then(toml::Value::as_array) {
        Some(patterns) => patterns
            .iter()
            .filter_map(toml::Value::as_str)
            .any(|pattern| cargo_member_pattern_matches(pattern, crate_relative_path)),
        // Cargo permits a package to be colocated with a `[workspace]` table
        // without listing the package in `members`. Prefer the nearest ancestor
        // manifest that defines a workspace because it still describes the
        // broader Cargo workspace for the analyzer.
        None => true,
    }
}

fn cargo_member_pattern_matches(pattern: &str, crate_relative_path: &Path) -> bool {
    globset::GlobBuilder::new(pattern.trim())
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher().is_match(crate_relative_path))
        .unwrap_or(false)
}

/// The highest directory, from `nearest` up to `project_root`, that holds one
/// of a server's root markers. `nearest` is returned when it lies outside the
/// project root or no higher directory has a marker.
pub fn outermost_marker_root(
    nearest: &Path,
    project_root: &Path,
    has_marker: impl Fn(&Path) -> bool,
) -> PathBuf {
    outermost_marker_root_with_mode(nearest, project_root, has_marker, RootPathMode::Resolve)
}

pub(crate) fn outermost_marker_root_with_mode(
    nearest: &Path,
    project_root: &Path,
    has_marker: impl Fn(&Path) -> bool,
    mode: RootPathMode,
) -> PathBuf {
    let project_root = mode.resolve(project_root);
    let mut chosen = nearest.to_path_buf();
    if !nearest.starts_with(&project_root) {
        return chosen;
    }
    let mut current = nearest.parent();
    while let Some(dir) = current {
        if !dir.starts_with(&project_root) {
            break;
        }
        if has_marker(dir) {
            chosen = dir.to_path_buf();
        }
        if dir == project_root {
            break;
        }
        current = dir.parent();
    }
    chosen
}

/// Whether a walk that chooses language servers skips this entry and
/// everything under it.
///
/// Dependency and build output directories hold no project sources. Test
/// fixtures, corpora and `spikes/` do hold sources, but they are inputs to
/// tests or throwaway experiments, often with their own manifests: each one
/// would start another server (a fixture `Cargo.toml` is a separate
/// rust-analyzer workspace; a spike with a stale `Cargo.lock` fails its
/// workspace load) whose diagnostics nobody asked for. The walk root itself
/// (`depth == 0`) is never skipped, so a scope that names such a directory
/// still gets its servers. Ignore files (`.gitignore`, `.aftignore`) are
/// applied by the walker itself.
pub fn skip_in_server_walk(name: &str, depth: usize) -> bool {
    if matches!(
        name,
        ".git" | "node_modules" | "target" | "dist" | "build" | ".next" | ".turbo"
    ) {
        return true;
    }
    depth > 0 && (name == "spikes" || crate::file_kind::is_test_support_dir_name(name))
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

    use super::{
        find_rust_workspace_root, find_workspace_root, find_workspace_root_within, ServerKey,
    };
    use crate::lsp::registry::ServerKind;
    use crate::path_identity::canonicalize_normalized;

    #[test]
    fn test_find_root_with_cargo_toml() {
        let temp_dir = tempdir().unwrap();
        let root = temp_dir.path().join("workspace");
        let src_dir = root.join("src");
        let file = src_dir.join("lib.rs");

        fs::create_dir_all(&src_dir).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(&file, "fn main() {}\n").unwrap();

        // Expectations go through the same normalization as production:
        // bare fs::canonicalize returns verbatim (\\?\) paths on Windows,
        // which find_workspace_root deliberately strips.
        let expected_root = crate::path_identity::canonicalize_normalized(&root);
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

        let expected_root = crate::path_identity::canonicalize_normalized(&crate_root);
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

        let expected_root = crate::path_identity::canonicalize_normalized(&root);
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

    /// Regression test for #174: the workspace root returned for a nested file
    /// must never carry a Windows verbatim (`\\?\`) prefix, because it flows
    /// into `LspClient::spawn` -> `Command::current_dir`, and `CreateProcess`
    /// rejects verbatim paths as `lpCurrentDirectory` (every LSP spawn on
    /// Windows would otherwise fail with "The system cannot find the path
    /// specified").
    ///
    /// This runs on every platform (not `cfg(windows)`-gated) because the
    /// normalization is platform-independent: on Unix `canonicalize_normalized`
    /// is identity-equivalent to `fs::canonicalize` plus lexical `.`/`..`
    /// collapse, so the assertion is a no-op there; on Windows (MSVC CI) it
    /// asserts the verbatim strip. The byte-equality check against
    /// `canonicalize_normalized` is the platform-independent property that
    /// fails locally on macOS if the roots.rs chokepoint is reverted to a bare
    /// `fs::canonicalize` (whose output diverges from the normalized form only
    /// on Windows, but the equality contract holds everywhere).
    #[test]
    fn test_find_root_strips_windows_verbatim_prefix() {
        let temp_dir = tempdir().unwrap();
        let root = temp_dir.path().join("workspace");
        let src_dir = root.join("src");
        let nested = src_dir.join("deep").join("lib.rs");

        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(&nested, "fn main() {}\n").unwrap();

        let found = find_workspace_root(&nested, &["Cargo.toml"]).expect("root found");

        // No verbatim prefix on any platform.
        let display = found.to_string_lossy();
        assert!(
            !display.starts_with("\\\\?\\"),
            "workspace root must not carry a Windows verbatim prefix: {display}"
        );

        // Byte-for-byte equality with the shared normalizer for the same
        // fixture. This is the platform-independent mutation control: reverting
        // roots.rs to a bare `fs::canonicalize` makes `find_workspace_root`
        // diverge from `canonicalize_normalized` on Windows, and on Unix the
        // equality still holds (both reduce to the canonical form), so the
        // test fails on Windows CI while passing locally on macOS.
        let expected = canonicalize_normalized(&root);
        assert_eq!(found, expected);
    }

    #[test]
    fn rust_workspace_root_prefers_owning_workspace_over_member_manifest() {
        let temp_dir = tempdir().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let crate_root = workspace.join("crates").join("member");
        let source = crate_root.join("src").join("lib.rs");

        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/member\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::write(
            crate_root.join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(&source, "pub fn answer() -> u32 { 42 }\n").unwrap();

        assert_eq!(
            find_rust_workspace_root(&source, Some(&workspace)),
            Some(canonicalize_normalized(&workspace))
        );
    }

    #[test]
    fn rust_workspace_root_keeps_crate_excluded_from_parent_workspace_standalone() {
        let temp_dir = tempdir().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let member_root = workspace.join("crates").join("member");
        let standalone_root = workspace.join("tools").join("standalone");
        let source = standalone_root.join("src").join("lib.rs");

        fs::create_dir_all(member_root.join("src")).unwrap();
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/member\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::write(
            member_root.join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            standalone_root.join("Cargo.toml"),
            "[package]\nname = \"standalone\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(&source, "pub fn answer() -> u32 { 42 }\n").unwrap();

        assert_eq!(
            find_rust_workspace_root(&source, Some(&workspace)),
            None,
            "the parent workspace does not list this crate"
        );
    }

    #[test]
    fn rust_workspace_root_does_not_walk_above_project_root() {
        let temp_dir = tempdir().unwrap();
        let outer_workspace = temp_dir.path().join("outer-workspace");
        let project_root = outer_workspace.join("nested-project");
        let crate_root = project_root.join("crate");
        let source = crate_root.join("src").join("lib.rs");

        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(
            outer_workspace.join("Cargo.toml"),
            "[workspace]\nmembers = [\"nested-project/crate\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        fs::write(
            crate_root.join("Cargo.toml"),
            "[package]\nname = \"nested\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(&source, "pub fn answer() -> u32 { 42 }\n").unwrap();
        let loose_source = project_root.join("loose.rs");
        fs::write(&loose_source, "pub fn loose() {}\n").unwrap();

        assert_eq!(
            find_rust_workspace_root(&source, Some(&project_root)),
            None,
            "the enclosing workspace belongs to a different session root"
        );
        assert_eq!(
            find_workspace_root_within(&loose_source, &["Cargo.toml"], Some(&project_root)),
            None,
            "marker lookup must not cross the session project root"
        );
    }
}

#[cfg(test)]
mod audit_regressions {
    #[test]
    fn cargo_member_star_stays_in_directory() {
        assert!(super::cargo_member_pattern_matches(
            "crates/*",
            std::path::Path::new("crates/a")
        ));
        assert!(!super::cargo_member_pattern_matches(
            "crates/*",
            std::path::Path::new("crates/a/b")
        ));
    }
    #[test]
    fn cargo_exclude_respects_literal_member_precedence() {
        let dir = tempfile::tempdir().unwrap();
        // Cargo metadata exited 0 for these manifests: literal x survived its
        // own exclude; glob x was excluded by x and by the parent crates path.
        for (members, exclude, x_expected, xyz_expected) in [
            ("crates/x", "crates/x", true, false),
            ("crates/*", "crates/x", false, true),
            ("crates/*", "crates", false, false),
        ] {
            std::fs::write(dir.path().join("Cargo.toml"), format!("[workspace]\nresolver=\"2\"\nmembers=[\"{members}\"]\nexclude=[\"{exclude}\"]\n")).unwrap();
            for (name, expected) in [("crates/x", x_expected), ("crates/xyz", xyz_expected)] {
                let root = dir.path().join(name);
                std::fs::create_dir_all(root.join("src")).unwrap();
                std::fs::write(root.join("src/lib.rs"), "").unwrap();
                std::fs::write(
                    root.join("Cargo.toml"),
                    format!(
                        "[package]\nname=\"{}\"\nversion=\"0.1.0\"\n",
                        name.replace('/', "-")
                    ),
                )
                .unwrap();
                assert_eq!(
                    super::cargo_workspace_contains_crate(dir.path(), &root),
                    expected,
                    "{members} / {exclude} / {name}"
                );
            }
        }
    }
}
